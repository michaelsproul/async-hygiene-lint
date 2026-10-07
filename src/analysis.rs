use crate::{
    ASYNC_HYGIENE_INCOMPLETE, DISALLOWED_FROM_ASYNC,
    config::{self, Config},
};
use rustc_errors::DiagDecorator;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_lint::LateContext;
use rustc_middle::{
    mir::{Body, Operand, Rvalue, StatementKind, TerminatorKind},
    ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypeVisitableExt, TypingEnv},
};
use rustc_span::Span;
use std::collections::{HashMap, VecDeque};

/// May-target sets for function pointers. References and aggregate fields are
/// conservatively merged; merely storing a callable does not create a call edge.
#[derive(Clone, Default, PartialEq, Eq)]
struct Value<'tcx>(Vec<Instance<'tcx>>);

impl<'tcx> Value<'tcx> {
    fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for target in &other.0 {
            if !self.0.contains(target) {
                self.0.push(*target);
                changed = true;
            }
        }
        changed
    }
}

#[derive(Clone)]
struct Edge {
    target: usize,
    span: Span,
}

struct Node<'tcx> {
    instance: Instance<'tcx>,
    inputs: Vec<Value<'tcx>>,
    output: Value<'tcx>,
    edges: Vec<Edge>,
    prohibited: Option<usize>,
    incomplete: Vec<String>,
}

struct Analysis<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    config: &'a Config,
    env: TypingEnv<'tcx>,
    nodes: Vec<Node<'tcx>>,
    indices: HashMap<Instance<'tcx>, usize>,
    changed: bool,
    limited: bool,
}

pub fn check<'tcx>(cx: &LateContext<'tcx>, config: &Config) {
    if config.prohibited.is_empty() {
        return;
    }
    let tcx = cx.tcx;
    for owner in tcx.hir_body_owners() {
        if !tcx.coroutine_is_async(owner.to_def_id()) {
            continue;
        }
        let instance = Instance::new_raw(
            owner.to_def_id(),
            ty::GenericArgs::identity_for_item(tcx, owner),
        );
        let mut analysis = Analysis {
            tcx,
            config,
            env: TypingEnv::post_analysis(tcx, owner),
            nodes: Vec::new(),
            indices: HashMap::new(),
            changed: false,
            limited: false,
        };
        analysis.node(instance, Vec::new());
        analysis.solve();
        analysis.report(owner);
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
        if let Some(&id) = self.indices.get(&instance) {
            let node = &mut self.nodes[id];
            node.inputs
                .resize_with(node.inputs.len().max(inputs.len()), Value::default);
            for (existing, incoming) in node.inputs.iter_mut().zip(inputs) {
                self.changed |= existing.join(&incoming);
            }
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
            .position(|r| config::matches(&r.path, &path));
        let id = self.nodes.len();
        self.indices.insert(instance, id);
        self.nodes.push(Node {
            instance,
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
            return Value(vec![Instance::new_raw(
                def_id,
                self.tcx.instantiate_bound_regions_with_erased(args),
            )]);
        }
        match operand {
            Operand::Copy(place) | Operand::Move(place) => locals[place.local.as_usize()].clone(),
            Operand::Constant(_) | Operand::RuntimeChecks(_) => Value::default(),
        }
    }

    fn scan(&mut self, id: usize) {
        if self.nodes[id].prohibited.is_some() {
            return;
        }
        let instance = self.nodes[id].instance;
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
        let mut locals = vec![Value::default(); body.local_decls.len()];
        for (local, input) in locals.iter_mut().skip(1).zip(&self.nodes[id].inputs) {
            local.join(input);
        }
        // Monotone, flow-insensitive may analysis within a body. Reassignment
        // retains both alternatives; loops and joins converge without recursion.
        loop {
            // Unresolved targets in an earlier round can become known after a
            // callee's return summary propagates. Report only the final round.
            self.nodes[id].incomplete.clear();
            let mut changed = false;
            for block in body.basic_blocks.iter() {
                for statement in &block.statements {
                    if let StatementKind::Assign(assignment) = &statement.kind {
                        let (place, rvalue) = &**assignment;
                        let mut value = Value::default();
                        match rvalue {
                            Rvalue::Use(op, _) | Rvalue::Cast(_, op, _) => {
                                value = self.operand(body, instance, &locals, op);
                            }
                            Rvalue::Ref(_, _, place) | Rvalue::RawPtr(_, place) => {
                                value = locals[place.local.as_usize()].clone();
                            }
                            Rvalue::Aggregate(_, operands) => {
                                for op in operands {
                                    value.join(&self.operand(body, instance, &locals, op));
                                }
                            }
                            _ => {}
                        }
                        changed |= locals[place.local.as_usize()].join(&value);
                    }
                }
                let terminator = block.terminator();
                match &terminator.kind {
                    TerminatorKind::Call {
                        func,
                        args,
                        destination,
                        ..
                    } => {
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
                        if targets.0.is_empty() {
                            self.incomplete(id, "unresolved function pointer call".into());
                        }
                        for target in targets.0 {
                            returned.join(&self.call(
                                id,
                                target,
                                values.clone(),
                                &types,
                                terminator.source_info.span,
                            ));
                        }
                        changed |= locals[destination.local.as_usize()].join(&returned);
                    }
                    TerminatorKind::Drop { place, .. } => {
                        let ty =
                            self.instantiate_ty(instance, place.ty(&body.local_decls, self.tcx).ty);
                        if !ty.has_non_region_param() && ty.needs_drop(self.tcx, self.env) {
                            let target = Instance::resolve_drop_glue(self.tcx, ty);
                            self.edge(
                                id,
                                target,
                                vec![locals[place.local.as_usize()].clone()],
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
        }
        self.changed |= self.nodes[id].output.join(&locals[0]);
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
        // Insulators are trusted execution contracts. Argument evaluation is
        // already represented by separate MIR statements/calls in the caller.
        if let Some(rule) = self
            .config
            .insulators
            .iter()
            .find(|r| config::matches(&r.path, &path))
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
                for &callback in &value.0 {
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
            self.changed = true;
        }
        self.nodes[id].output.clone()
    }

    fn report(&self, owner: LocalDefId) {
        let hir_id = self.tcx.local_def_id_to_hir_id(owner);
        let root_span = self.tcx.def_span(owner);
        let mut previous = vec![None; self.nodes.len()];
        let mut visited = vec![false; self.nodes.len()];
        let mut queue = VecDeque::from([0]);
        visited[0] = true;
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
