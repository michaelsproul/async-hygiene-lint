use crate::{
    ASYNC_HYGIENE_INCOMPLETE, DISALLOWED_FROM_ASYNC,
    config::{self, Config},
};
use rustc_errors::DiagDecorator;
use rustc_hir::{
    def::DefKind,
    def_id::{DefId, LocalDefId},
};
use rustc_lint::LateContext;
use rustc_middle::{
    mir::{
        AggregateKind, Body, Operand, Place, PlaceElem, ProjectionElem, Rvalue, StatementKind,
        TerminatorKind,
    },
    ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypeVisitableExt, TypingEnv},
};
use rustc_span::Span;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Callable provenance is field-sensitive. References retain the pointee's
/// abstract value; array indices conservatively join all possible elements.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct Value<'tcx> {
    targets: Vec<Instance<'tcx>>,
    fields: BTreeMap<usize, Value<'tcx>>,
    truncated: bool,
}

impl<'tcx> Value<'tcx> {
    fn function(target: Instance<'tcx>) -> Self {
        Self {
            targets: vec![target],
            ..Self::default()
        }
    }

    fn is_truncated(&self) -> bool {
        self.truncated || self.fields.values().any(Self::is_truncated)
    }

    fn join(&mut self, other: &Self) -> bool {
        self.join_at_depth(other, 0)
    }

    fn join_at_depth(&mut self, other: &Self, depth: usize) -> bool {
        let mut changed = false;
        for target in &other.targets {
            if !self.targets.contains(target) {
                self.targets.push(*target);
                changed = true;
            }
        }
        let truncated = other.truncated || (depth >= 8 && !other.fields.is_empty());
        if truncated && !self.truncated {
            self.truncated = true;
            changed = true;
        }
        if depth < 8 {
            for (field, value) in &other.fields {
                changed |= self
                    .fields
                    .entry(*field)
                    .or_default()
                    .join_at_depth(value, depth + 1);
            }
        }
        changed
    }

    fn projected(&self, projection: &[PlaceElem<'tcx>]) -> Self {
        let Some((first, rest)) = projection.split_first() else {
            return self.clone();
        };
        match first {
            ProjectionElem::Field(field, _) => self
                .fields
                .get(&field.as_usize())
                .map_or_else(Self::default, |value| value.projected(rest)),
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {
                let mut value = Self::default();
                for field in self.fields.values() {
                    value.join(&field.projected(rest));
                }
                value
            }
            _ => self.projected(rest),
        }
    }

    fn assign(&mut self, projection: &[PlaceElem<'tcx>], value: &Self) -> bool {
        let Some((first, rest)) = projection.split_first() else {
            return self.join(value);
        };
        match first {
            ProjectionElem::Field(field, _) => self
                .fields
                .entry(field.as_usize())
                .or_default()
                .assign(rest, value),
            // Array indices are represented by one joined element for writes.
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {
                self.fields.entry(0).or_default().assign(rest, value)
            }
            _ => self.assign(rest, value),
        }
    }
}

fn place_value<'tcx>(locals: &[Value<'tcx>], place: &Place<'tcx>) -> Value<'tcx> {
    locals[place.local.as_usize()].projected(place.projection)
}

#[derive(Clone, PartialEq, Eq)]
struct Edge {
    target: usize,
    span: Span,
}

struct Node<'tcx> {
    instance: Instance<'tcx>,
    env: TypingEnv<'tcx>,
    inputs: Vec<Value<'tcx>>,
    output: Value<'tcx>,
    edges: Vec<Edge>,
    prohibited: Option<usize>,
    incomplete: Vec<String>,
}

struct Analysis<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    config: &'a Config,
    prohibited_ids: &'a [Vec<DefId>],
    insulator_ids: &'a [Vec<DefId>],
    env: TypingEnv<'tcx>,
    nodes: Vec<Node<'tcx>>,
    indices: HashMap<(Instance<'tcx>, Vec<Value<'tcx>>, TypingEnv<'tcx>), usize>,
    changed: bool,
    limited: bool,
}

pub fn check<'tcx>(cx: &LateContext<'tcx>, config: &Config) {
    if config.prohibited.is_empty() {
        return;
    }
    let tcx = cx.tcx;
    let prohibited_ids: Vec<_> = config
        .prohibited
        .iter()
        .map(|rule| crate::paths::resolve(tcx, &rule.path))
        .collect();
    let insulator_ids: Vec<_> = config
        .insulators
        .iter()
        .map(|rule| crate::paths::resolve(tcx, &rule.path))
        .collect();
    let mut analysis = Analysis {
        tcx,
        config,
        prohibited_ids: &prohibited_ids,
        insulator_ids: &insulator_ids,
        env: TypingEnv::fully_monomorphized(),
        nodes: Vec::new(),
        indices: HashMap::new(),
        changed: false,
        limited: false,
    };
    let mut async_owners = Vec::new();
    for owner in tcx.hir_body_owners() {
        let is_async = tcx.coroutine_is_async(owner.to_def_id())
            || (matches!(tcx.def_kind(owner), DefKind::AssocFn)
                && tcx.item_name(owner.to_def_id()).as_str() == "poll"
                && tcx
                    .trait_item_of(owner)
                    .and_then(|item| tcx.trait_of_assoc(item))
                    .is_some_and(|trait_id| Some(trait_id) == tcx.lang_items().future_trait()));
        if is_async {
            async_owners.push(owner);
        }
        // Seed function bodies to discover concrete coroutine captures even if
        // the future is created in synchronous code or behind a generic helper.
        if !is_async && !matches!(tcx.def_kind(owner), DefKind::Fn | DefKind::AssocFn) {
            continue;
        }
        analysis.env = TypingEnv::post_analysis(tcx, owner);
        let instance = Instance::new_raw(
            owner.to_def_id(),
            ty::GenericArgs::identity_for_item(tcx, owner),
        );
        analysis.node(instance, Vec::new());
    }
    analysis.solve();
    for owner in async_owners {
        let mut roots: Vec<_> = analysis
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.instance.def_id() == owner.to_def_id())
            .map(|(id, _)| id)
            .collect();
        // Prefer actual construction contexts over the open-world seed. The
        // seed still checks unconstructed async functions and generic libraries.
        if roots
            .iter()
            .any(|&id| !analysis.nodes[id].inputs.is_empty())
        {
            roots.retain(|&id| !analysis.nodes[id].inputs.is_empty());
        }
        analysis.report(owner, &roots);
    }
}

impl<'a, 'tcx> Analysis<'a, 'tcx> {
    fn path(&self, def_id: DefId) -> String {
        let path = self.tcx.def_path_str(def_id);
        if def_id.is_local() {
            format!("{}::{path}", self.tcx.crate_name(def_id.krate))
        } else {
            path
        }
    }

    fn node(&mut self, instance: Instance<'tcx>, inputs: Vec<Value<'tcx>>) -> Option<usize> {
        if let Some(&id) = self.indices.get(&(instance, inputs.clone(), self.env)) {
            return Some(id);
        }
        if self.nodes.len() >= self.config.max_instances {
            self.limited = true;
            return None;
        }
        let path = self.path(instance.def_id());
        let prohibited = self
            .config
            .prohibited
            .iter()
            .enumerate()
            .position(|(index, r)| {
                self.prohibited_ids[index].contains(&instance.def_id())
                    || config::matches(&r.path, &path)
            });
        let id = self.nodes.len();
        self.indices
            .insert((instance, inputs.clone(), self.env), id);
        self.nodes.push(Node {
            instance,
            env: self.env,
            inputs,
            output: Value::default(),
            edges: Vec::new(),
            prohibited,
            incomplete: Vec::new(),
        });
        self.changed = true;
        Some(id)
    }

    fn solve(&mut self) {
        for _ in 0..self.config.max_iterations {
            self.changed = false;
            let mut id = 0;
            // Discover callees in the same pass; the next pass propagates returns
            // back through callers and closes recursive strongly connected components.
            while id < self.nodes.len() {
                self.scan(id);
                id += 1;
            }
            if !self.changed {
                return;
            }
        }
        self.limited = true;
    }

    fn incomplete(&mut self, id: usize, message: String) {
        if !self.nodes[id].incomplete.contains(&message) {
            self.nodes[id].incomplete.push(message.clone());
        }
    }

    fn instantiate_ty(&self, instance: Instance<'tcx>, ty: Ty<'tcx>) -> Ty<'tcx> {
        instance
            .try_instantiate_mir_and_normalize_erasing_regions(
                self.tcx,
                self.env,
                EarlyBinder::bind(self.tcx, ty),
            )
            .unwrap_or_else(|_| instance.instantiate_mir(self.tcx, EarlyBinder::bind(self.tcx, ty)))
    }

    fn operand(
        &self,
        body: &Body<'tcx>,
        instance: Instance<'tcx>,
        locals: &[Value<'tcx>],
        operand: &Operand<'tcx>,
    ) -> Value<'tcx> {
        let ty = self.instantiate_ty(instance, operand.ty(&body.local_decls, self.tcx));
        if let ty::FnDef(def_id, args) = *ty.kind() {
            return Value::function(Instance::new_raw(
                def_id,
                self.tcx.instantiate_bound_regions_with_erased(args),
            ));
        }
        match operand {
            Operand::Copy(place) | Operand::Move(place) => place_value(locals, place),
            Operand::Constant(_) | Operand::RuntimeChecks(_) => Value::default(),
        }
    }

    fn scan(&mut self, id: usize) {
        if self.nodes[id].prohibited.is_some() {
            return;
        }
        let instance = self.nodes[id].instance;
        self.env = self.nodes[id].env;
        match instance.def {
            InstanceKind::Intrinsic(_) | InstanceKind::LlvmIntrinsic(_) => return,
            InstanceKind::Virtual(..) => {
                self.incomplete(
                    id,
                    format!("dynamic dispatch to `{}`", self.path(instance.def_id())),
                );
                return;
            }
            InstanceKind::Item(def_id) if !self.tcx.is_mir_available(def_id) => {
                self.incomplete(
                    id,
                    format!(
                        "MIR unavailable for `{}` (compile dependencies with -Zalways-encode-mir)",
                        self.path(def_id)
                    ),
                );
                return;
            }
            _ => {}
        }
        let body = self.tcx.instance_mir(instance.def);
        let previous_edges = self.nodes[id].edges.clone();
        let mut locals = vec![Value::default(); body.local_decls.len()];
        for (local, input) in locals.iter_mut().skip(1).zip(&self.nodes[id].inputs) {
            local.join(input);
        }
        // Monotone, flow-insensitive may analysis within a body. Reassignment
        // retains both alternatives; loops and joins converge without recursion.
        for round in 0..self.config.max_iterations {
            // Unresolved targets in an earlier round can become known after a
            // callee's return summary propagates. Report only the final round.
            self.nodes[id].incomplete.clear();
            self.nodes[id].edges.clear();
            let mut changed = false;
            for block in body.basic_blocks.iter() {
                for statement in &block.statements {
                    if let StatementKind::Assign(assignment) = &statement.kind {
                        let (place, rvalue) = &**assignment;
                        let mut value = Value::default();
                        match rvalue {
                            Rvalue::Use(op, _) => {
                                value = self.operand(body, instance, &locals, op);
                            }
                            Rvalue::Cast(_, op, _) => {
                                value = self.operand(body, instance, &locals, op);
                                let ty = self
                                    .instantiate_ty(instance, op.ty(&body.local_decls, self.tcx));
                                if let ty::Closure(def_id, args) = *ty.kind() {
                                    value.targets.push(Instance::new_raw(def_id, args));
                                }
                            }
                            Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => {
                                value = place_value(&locals, place);
                            }
                            Rvalue::Aggregate(kind, operands) => {
                                for (index, op) in operands.iter().enumerate() {
                                    let field = self.operand(body, instance, &locals, op);
                                    if field != Value::default() {
                                        value.fields.insert(index, field);
                                    }
                                }
                                if let AggregateKind::Coroutine(def_id, args) = **kind {
                                    if def_id.is_local() {
                                        let coroutine_ty =
                                            Ty::new_coroutine(self.tcx, def_id, args);
                                        let coroutine_ty =
                                            self.instantiate_ty(instance, coroutine_ty);
                                        if let ty::Coroutine(def_id, args) = *coroutine_ty.kind() {
                                            // Lowered Future::poll receives Pin<&mut Self>.
                                            let mut pinned = Value::default();
                                            pinned.fields.insert(0, value.clone());
                                            self.node(
                                                Instance::new_raw(def_id, args),
                                                vec![pinned, Value::default()],
                                            );
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        changed |= locals[place.local.as_usize()].assign(place.projection, &value);
                    }
                }
                let terminator = block.terminator();
                match &terminator.kind {
                    TerminatorKind::Call { func, args, .. }
                    | TerminatorKind::TailCall { func, args, .. } => {
                        let values: Vec<_> = args
                            .iter()
                            .map(|a| self.operand(body, instance, &locals, &a.node))
                            .collect();
                        let types: Vec<_> = args
                            .iter()
                            .map(|a| {
                                self.instantiate_ty(
                                    instance,
                                    a.node.ty(&body.local_decls, self.tcx),
                                )
                            })
                            .collect();
                        let targets = self.operand(body, instance, &locals, func);
                        let mut returned = Value::default();
                        if targets.targets.is_empty() {
                            self.incomplete(id, "unresolved function pointer call".into());
                        }
                        for target in targets.targets {
                            returned.join(&self.call(
                                id,
                                target,
                                values.clone(),
                                &types,
                                terminator.source_info.span,
                            ));
                        }
                        if let TerminatorKind::Call { destination, .. } = terminator.kind {
                            changed |= locals[destination.local.as_usize()]
                                .assign(destination.projection, &returned);
                        } else {
                            changed |= locals[0].join(&returned);
                        }
                    }
                    TerminatorKind::Drop { place, .. } => {
                        let ty =
                            self.instantiate_ty(instance, place.ty(&body.local_decls, self.tcx).ty);
                        if !ty.has_non_region_param() && ty.needs_drop(self.tcx, self.env) {
                            let target = Instance::resolve_drop_glue(self.tcx, ty);
                            self.edge(
                                id,
                                target,
                                vec![place_value(&locals, place)],
                                terminator.source_info.span,
                            );
                        }
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
            if round + 1 == self.config.max_iterations {
                self.incomplete(
                    id,
                    "function-pointer dataflow iteration limit reached".into(),
                );
            }
        }
        if locals.iter().any(Value::is_truncated) {
            self.incomplete(id, "callable aggregate nesting limit reached".into());
        }
        self.changed |= self.nodes[id].output.join(&locals[0]);
        self.changed |= previous_edges != self.nodes[id].edges;
    }

    fn call(
        &mut self,
        caller: usize,
        target: Instance<'tcx>,
        values: Vec<Value<'tcx>>,
        types: &[Ty<'tcx>],
        span: Span,
    ) -> Value<'tcx> {
        let path = self.path(target.def_id());
        if self
            .config
            .prohibited
            .iter()
            .enumerate()
            .any(|(index, rule)| {
                self.prohibited_ids[index].contains(&target.def_id())
                    || config::matches(&rule.path, &path)
            })
        {
            return self.edge(caller, target, values, span);
        }
        // Insulators are trusted execution contracts. Argument evaluation is
        // already represented by separate MIR statements/calls in the caller.
        if let Some((_, rule)) = self
            .config
            .insulators
            .iter()
            .enumerate()
            .find(|(index, r)| {
                self.insulator_ids[*index].contains(&target.def_id())
                    || config::matches(&r.path, &path)
            })
        {
            let insulated = rule.callback_args.clone();
            if insulated.iter().any(|&index| index >= values.len()) {
                self.incomplete(
                    caller,
                    format!("insulator `{path}` has an out-of-range callback argument"),
                );
            }
            for (index, value) in values.iter().enumerate() {
                if insulated.contains(&index) {
                    continue;
                }
                for &callback in &value.targets {
                    self.edge(caller, callback, Vec::new(), span);
                }
                if let ty::Closure(def_id, args) = *types[index].peel_refs().kind() {
                    self.edge(
                        caller,
                        Instance::new_raw(def_id, args),
                        vec![value.clone()],
                        span,
                    );
                }
            }
            return Value::default();
        }
        match Instance::try_resolve(self.tcx, self.env, target.def_id(), target.args) {
            Ok(Some(resolved)) => self.edge(caller, resolved, values, span),
            _ => {
                self.incomplete(caller, format!("unresolved generic call to `{path}`"));
                Value::default()
            }
        }
    }

    fn edge(
        &mut self,
        caller: usize,
        target: Instance<'tcx>,
        values: Vec<Value<'tcx>>,
        span: Span,
    ) -> Value<'tcx> {
        let Some(id) = self.node(target, values) else {
            return Value::default();
        };
        if !self.nodes[caller]
            .edges
            .iter()
            .any(|edge| edge.target == id && edge.span == span)
        {
            self.nodes[caller].edges.push(Edge { target: id, span });
        }
        self.nodes[id].output.clone()
    }

    fn report(&self, owner: LocalDefId, roots: &[usize]) {
        let hir_id = self.tcx.local_def_id_to_hir_id(owner);
        let root_span = self.tcx.def_span(owner);
        let mut previous = vec![None; self.nodes.len()];
        let mut visited = vec![false; self.nodes.len()];
        let mut queue = VecDeque::from(roots.to_vec());
        for &root in roots {
            visited[root] = true;
        }
        let mut reported = vec![false; self.config.prohibited.len()];
        let mut incomplete = Vec::new();
        while let Some(id) = queue.pop_front() {
            let node = &self.nodes[id];
            if let Some(rule_index) = node.prohibited {
                if !reported[rule_index] {
                    reported[rule_index] = true;
                    let rule = &self.config.prohibited[rule_index];
                    let mut chain = Vec::new();
                    let mut cursor = id;
                    while let Some((parent, span)) = previous[cursor] {
                        chain.push((cursor, span));
                        cursor = parent;
                    }
                    chain.reverse();
                    let span = chain.first().map_or(root_span, |(_, span)| *span);
                    self.tcx.emit_node_span_lint(DISALLOWED_FROM_ASYNC, hir_id, span, DiagDecorator(|diag| {
                        diag.primary_message(format!("async context calls prohibited function `{}`", self.path(node.instance.def_id())));
                        if !rule.reason.is_empty() { diag.note(rule.reason.clone()); }
                        for (target, span) in &chain {
                            diag.span_note(*span, format!("calls `{}`", self.path(self.nodes[*target].instance.def_id())));
                        }
                        diag.help("move the blocking operation into a configured blocking-thread callback");
                    }));
                }
            }
            for message in &node.incomplete {
                if !incomplete.contains(message) {
                    incomplete.push(message.clone());
                }
            }
            for edge in &node.edges {
                if !visited[edge.target] {
                    visited[edge.target] = true;
                    previous[edge.target] = Some((id, edge.span));
                    queue.push_back(edge.target);
                }
            }
        }
        if self.limited || !incomplete.is_empty() {
            self.tcx.emit_node_span_lint(
                ASYNC_HYGIENE_INCOMPLETE,
                hir_id,
                root_span,
                DiagDecorator(|diag| {
                    diag.primary_message(
                        "async hygiene analysis is incomplete for this async context",
                    );
                    for message in incomplete.iter().take(5) {
                        diag.note((*message).clone());
                    }
                    if incomplete.len() > 5 {
                        diag.note(format!(
                            "{} additional unresolved calls",
                            incomplete.len() - 5
                        ));
                    }
                    if self.limited {
                        diag.note(
                            "analysis work limit reached; increase max-instances or max-iterations",
                        );
                    }
                }),
            );
        }
    }
}
