use crate::{
    ASYNC_HYGIENE_INCOMPLETE, DISALLOWED_FROM_ASYNC,
    config::{self, Config, Limit},
    value::{Policy, Value as Provenance},
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
use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    time::Instant,
};

type Value<'tcx> = Provenance<Instance<'tcx>>;

fn place_value<'tcx>(locals: &[Value<'tcx>], place: &Place<'tcx>, policy: &Policy) -> Value<'tcx> {
    let mut value = locals[place.local.as_usize()].clone();
    for elem in place.projection {
        match elem {
            ProjectionElem::Field(field, _) => {
                value = value.project(Some(field.as_usize()), policy)
            }
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => {
                value = value.project(None, policy)
            }
            _ => {}
        }
    }
    value
}

fn assign<'tcx>(
    value: &mut Value<'tcx>,
    projection: &[PlaceElem<'tcx>],
    other: &Value<'tcx>,
    policy: &Policy,
) -> bool {
    let path: Vec<_> = projection
        .iter()
        .filter_map(|elem| match elem {
            ProjectionElem::Field(field, _) => Some(field.as_usize()),
            ProjectionElem::Index(_) | ProjectionElem::ConstantIndex { .. } => Some(0),
            _ => None,
        })
        .collect();
    value.assign(&path, other, policy)
}

/// Count unique type components, skipping shared subtrees. Expanding `(T, T)`
/// recursively must not make the work-limit check itself exponential.
fn type_size(instance: Instance<'_>) -> usize {
    let mut seen = HashSet::new();
    for generic in instance.args {
        let mut walk = generic.walk();
        while let Some(arg) = walk.next() {
            if !seen.insert(arg) {
                walk.skip_current_subtree();
            }
        }
    }
    seen.len()
}

/// Budget failures stay separate from unsupported MIR/dispatch diagnostics.
#[derive(Clone, PartialEq, Eq)]
struct Exhaustion {
    key: &'static str,
    limit: Limit,
    observed: usize,
    context: String,
}

impl Exhaustion {
    fn message(&self) -> String {
        format!(
            "{}={} {}; observed/attempted count {}",
            self.key, self.limit, self.context, self.observed
        )
    }
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
    discovered: Vec<usize>,
    prohibited: Option<usize>,
    incomplete: Vec<String>,
    exhausted: Vec<Exhaustion>,
    ancestry: Vec<Instance<'tcx>>,
}

#[derive(Default)]
struct Statistics {
    instances: usize,
    instance_attempt: usize,
    solver_iterations: usize,
    dataflow_iterations: usize,
    recursive_instances: usize,
    solver_converged: bool,
    exhausted: BTreeSet<&'static str>,
}

impl Statistics {
    fn report(
        &self,
        tcx: TyCtxt<'_>,
        depth: usize,
        dataflow_converged: bool,
        started: Instant,
        skipped: bool,
    ) {
        tcx.dcx().note(format!(
            "async hygiene statistics for `{}`: instances={}; peak-instance-attempt={}; solver-iterations={}; peak-dataflow-iterations={}; peak-aggregate-depth={}; peak-recursive-instances={}; solver-converged={}; dataflow-converged={}; exhausted-budgets=[{}]; elapsed-ms={:.3}; skipped={}",
            tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE), self.instances,
            self.instance_attempt, self.solver_iterations, self.dataflow_iterations,
            depth, self.recursive_instances, self.solver_converged, dataflow_converged,
            self.exhausted.iter().copied().collect::<Vec<_>>().join(","),
            started.elapsed().as_secs_f64() * 1000.0,
            if skipped { "no-local-async-entry-points" } else { "no" },
        ));
    }
}

fn is_async_owner(tcx: TyCtxt<'_>, owner: LocalDefId) -> bool {
    tcx.coroutine_is_async(owner.to_def_id())
        || (matches!(tcx.def_kind(owner), DefKind::AssocFn)
            && tcx.item_name(owner.to_def_id()).as_str() == "poll"
            && tcx
                .trait_item_of(owner)
                .and_then(|item| tcx.trait_of_assoc(item))
                .is_some_and(|trait_id| Some(trait_id) == tcx.lang_items().future_trait()))
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
    exhausted: Vec<Exhaustion>,
    policy: Policy,
    statistics: Statistics,
    current: Option<usize>,
}

pub fn check<'tcx>(cx: &LateContext<'tcx>, config: &Config) {
    if config.prohibited.is_empty() {
        return;
    }
    let started = Instant::now();
    let tcx = cx.tcx;
    // Discover entry points before seeding synchronous functions. A crate with
    // none has no local diagnostics to report, even if its sync code expands.
    let owners: Vec<_> = tcx.hir_body_owners().collect();
    let async_owners: Vec<_> = owners
        .iter()
        .copied()
        .filter(|&owner| is_async_owner(tcx, owner))
        .collect();
    if async_owners.is_empty() {
        if config.statistics {
            Statistics::default().report(tcx, 0, false, started, true);
        }
        return;
    }
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
        exhausted: Vec::new(),
        policy: Policy {
            depth: config.max_aggregate_depth,
            observed_depth: Cell::new(0),
        },
        current: None,
        statistics: Statistics::default(),
    };
    let mut seeds = Vec::new();
    for owner in owners {
        let is_async = async_owners.contains(&owner);
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
        if let Some(id) = analysis.node(instance, Vec::new()) {
            seeds.push(id);
        }
    }
    analysis.solve();
    // Discard provisional construction contexts from earlier fixed-point
    // rounds, such as a future created before its factory's return is known.
    let mut live = vec![false; analysis.nodes.len()];
    while let Some(id) = seeds.pop() {
        if std::mem::replace(&mut live[id], true) {
            continue;
        }
        seeds.extend(analysis.nodes[id].edges.iter().map(|edge| edge.target));
        seeds.extend(analysis.nodes[id].discovered.iter().copied());
    }
    for &owner in &async_owners {
        let mut roots: Vec<_> = analysis
            .nodes
            .iter()
            .enumerate()
            .filter(|(id, node)| live[*id] && node.instance.def_id() == owner.to_def_id())
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
        analysis.report(owner, &roots, &async_owners);
    }
    if config.statistics {
        let dataflow_converged = !analysis.nodes.iter().any(|node| {
            node.exhausted
                .iter()
                .any(|reason| reason.key == "max-dataflow-iterations")
        });
        analysis.statistics.report(
            tcx,
            analysis.policy.observed_depth.get(),
            dataflow_converged,
            started,
            false,
        );
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
        if self.current.is_some() && inputs.iter().any(Value::is_truncated) {
            self.aggregate_exhaustion();
        }
        if let Some(&id) = self.indices.get(&(instance, inputs.clone(), self.env)) {
            return Some(id);
        }
        self.statistics.instance_attempt = self
            .statistics
            .instance_attempt
            .max(self.nodes.len().saturating_add(1));
        if self.config.max_instances.exhausted(self.nodes.len()) {
            // The allocation pool is shared with coroutine/capture discovery
            // in synchronous bodies. A rejected instance can hide a root that
            // has no call edge from the async contexts we managed to discover.
            self.global_exhaustion(Exhaustion {
                key: "max-instances",
                limit: self.config.max_instances,
                observed: self.nodes.len().saturating_add(1),
                context: format!(
                    "prevented allocation of `{}`{}",
                    self.path(instance.def_id()),
                    self.current.map_or_else(
                        || " during root discovery".into(),
                        |id| format!(
                            " called from `{}`",
                            self.path(self.nodes[id].instance.def_id())
                        )
                    )
                ),
            });
            return None;
        }
        let mut ancestry = self
            .current
            .map_or_else(Vec::new, |id| self.nodes[id].ancestry.clone());
        // Polymorphic recursion can create infinitely many distinct instances
        // (and exponentially growing types), even in uncalled Rust functions.
        // Ordinary recursion reuses an existing key above and is unaffected.
        let recursive: Vec<_> = ancestry
            .iter()
            .filter(|ancestor| ancestor.def_id() == instance.def_id())
            .collect();
        if matches!(instance.def, InstanceKind::Item(_)) {
            self.statistics.recursive_instances = self
                .statistics
                .recursive_instances
                .max(recursive.len().saturating_add(1));
        }
        if matches!(instance.def, InstanceKind::Item(_))
            && self
                .config
                .max_recursive_instances
                .exhausted(recursive.len())
            && type_size(instance) >= type_size(**recursive.last().unwrap())
        {
            self.exhaustion(Exhaustion {
                key: "max-recursive-instances",
                limit: self.config.max_recursive_instances,
                observed: recursive.len().saturating_add(1),
                context: format!(
                    "prevented non-shrinking expansion of `{}` along the current ancestry",
                    self.path(instance.def_id())
                ),
            });
            return None;
        }
        ancestry.push(instance);
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
            discovered: Vec::new(),
            prohibited,
            incomplete: Vec::new(),
            exhausted: Vec::new(),
            ancestry,
        });
        self.statistics.instances = self.nodes.len();
        self.changed = true;
        Some(id)
    }

    fn discover(&mut self, caller: usize, instance: Instance<'tcx>, inputs: Vec<Value<'tcx>>) {
        if let Some(id) = self.node(instance, inputs) {
            if !self.nodes[caller].discovered.contains(&id) {
                self.nodes[caller].discovered.push(id);
            }
        }
    }

    fn solve(&mut self) {
        let mut rounds = 0usize;
        loop {
            self.changed = false;
            let mut id = 0;
            // Discover callees in the same pass; the next pass propagates returns
            // back through callers and closes recursive strongly connected components.
            while id < self.nodes.len() {
                self.scan(id);
                id += 1;
            }
            rounds = rounds.saturating_add(1);
            self.statistics.solver_iterations = rounds;
            if !self.changed {
                self.statistics.solver_converged = true;
                return;
            }
            if self.config.max_iterations.exhausted(rounds) {
                self.global_exhaustion(Exhaustion {
                    key: "max-iterations",
                    limit: self.config.max_iterations,
                    observed: rounds,
                    context: "stopped the crate-wide solver before convergence".into(),
                });
                return;
            }
        }
    }

    fn exhaustion(&mut self, reason: Exhaustion) {
        self.statistics.exhausted.insert(reason.key);
        let reasons = match self.current {
            Some(id) => &mut self.nodes[id].exhausted,
            None => &mut self.exhausted,
        };
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }

    fn global_exhaustion(&mut self, reason: Exhaustion) {
        self.statistics.exhausted.insert(reason.key);
        if !self.exhausted.contains(&reason) {
            self.exhausted.push(reason);
        }
    }

    fn aggregate_exhaustion(&mut self) {
        if let Limit::Finite(limit) = self.config.max_aggregate_depth {
            self.exhaustion(Exhaustion {
                key: "max-aggregate-depth",
                limit: self.config.max_aggregate_depth,
                observed: limit.saturating_add(1),
                context: format!(
                    "prevented propagation at field depth {} in `{}`",
                    limit.saturating_add(1),
                    self.path(self.nodes[self.current.unwrap()].instance.def_id())
                ),
            });
        }
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
            Operand::Copy(place) | Operand::Move(place) => place_value(locals, place, &self.policy),
            Operand::Constant(_) if matches!(ty.kind(), ty::FnPtr(..)) => {
                let mut value = Value::default();
                value.root.unknown = true;
                value
            }
            Operand::Constant(_) | Operand::RuntimeChecks(_) => Value::default(),
        }
    }

    fn scan(&mut self, id: usize) {
        self.current = Some(id);
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
                let cause = if matches!(
                    self.tcx.crate_name(def_id.krate).as_str(),
                    "std" | "core" | "alloc"
                ) {
                    "precompiled standard library"
                } else if self.tcx.is_foreign_item(def_id) {
                    "external/FFI implementation"
                } else {
                    "compile dependencies with -Zalways-encode-mir"
                };
                self.incomplete(
                    id,
                    format!("MIR unavailable for `{}` ({cause})", self.path(def_id)),
                );
                return;
            }
            _ => {}
        }
        let body = self.tcx.instance_mir(instance.def);
        let previous_edges = self.nodes[id].edges.clone();
        let previous_discovered = self.nodes[id].discovered.clone();
        let mut locals = vec![Value::default(); body.local_decls.len()];
        for (local, input) in locals.iter_mut().skip(1).zip(&self.nodes[id].inputs) {
            local.join(input, &self.policy);
        }
        for argument in (self.nodes[id].inputs.len() + 1)..=body.arg_count {
            let ty = self.instantiate_ty(
                instance,
                body.local_decls[rustc_middle::mir::Local::from_usize(argument)].ty,
            );
            if matches!(ty.peel_refs().kind(), ty::FnPtr(..)) {
                locals[argument].root.unknown = true;
            }
        }
        // Monotone, flow-insensitive may analysis within a body. Reassignment
        // retains both alternatives; loops and joins converge without recursion.
        let mut rounds = 0usize;
        loop {
            // Unresolved targets in an earlier round can become known after a
            // callee's return summary propagates. Report only the final round.
            self.nodes[id].incomplete.clear();
            self.nodes[id].exhausted.clear();
            self.nodes[id].edges.clear();
            self.nodes[id].discovered.clear();
            let mut changed = false;
            for block in body.basic_blocks.iter() {
                for statement in &block.statements {
                    if let StatementKind::Assign(assignment) = &statement.kind {
                        let (place, rvalue) = &**assignment;
                        let mut value = Value::default();
                        match rvalue {
                            Rvalue::Use(op, _) | Rvalue::WrapUnsafeBinder(op, _) => {
                                value = self.operand(body, instance, &locals, op);
                            }
                            Rvalue::Cast(_, op, _) => {
                                value = self.operand(body, instance, &locals, op);
                                let ty = self
                                    .instantiate_ty(instance, op.ty(&body.local_decls, self.tcx));
                                if let ty::Closure(def_id, args) = *ty.kind() {
                                    value.root.targets.push(Instance::new_raw(def_id, args));
                                }
                            }
                            Rvalue::Ref(_, _, place)
                            | Rvalue::RawPtr(_, place)
                            | Rvalue::CopyForDeref(place)
                            | Rvalue::Reborrow(_, _, place) => {
                                value = place_value(&locals, place, &self.policy);
                            }
                            Rvalue::Repeat(op, _) => {
                                value.assign(
                                    &[0],
                                    &self.operand(body, instance, &locals, op),
                                    &self.policy,
                                );
                            }
                            Rvalue::Aggregate(kind, operands) => {
                                for (index, op) in operands.iter().enumerate() {
                                    let field = self.operand(body, instance, &locals, op);
                                    if field != Value::default() {
                                        value.assign(&[index], &field, &self.policy);
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
                                            pinned.assign(&[0], &value, &self.policy);
                                            self.discover(
                                                id,
                                                Instance::new_raw(def_id, args),
                                                vec![pinned, Value::default()],
                                            );
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        changed |= assign(
                            &mut locals[place.local.as_usize()],
                            place.projection,
                            &value,
                            &self.policy,
                        );
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
                        if targets.root.targets.is_empty() || targets.root.unknown {
                            self.incomplete(id, "unresolved function pointer call".into());
                        }
                        for target in targets.root.targets {
                            let output = self.call(
                                id,
                                target,
                                values.clone(),
                                &types,
                                terminator.source_info.span,
                            );
                            returned.join(&output, &self.policy);
                        }
                        if let TerminatorKind::Call { destination, .. } = terminator.kind {
                            changed |= assign(
                                &mut locals[destination.local.as_usize()],
                                destination.projection,
                                &returned,
                                &self.policy,
                            );
                        } else {
                            changed |= locals[0].join(&returned, &self.policy);
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
                                vec![place_value(&locals, place, &self.policy)],
                                terminator.source_info.span,
                            );
                        } else if ty.has_non_region_param() && ty.needs_drop(self.tcx, self.env) {
                            self.incomplete(
                                id,
                                format!("unresolved generic destructor for `{ty}`"),
                            );
                        }
                    }
                    _ => {}
                }
            }
            rounds = rounds.saturating_add(1);
            if !changed {
                break;
            }
            if self.config.dataflow_iterations().exhausted(rounds) {
                self.exhaustion(Exhaustion {
                    key: "max-dataflow-iterations",
                    limit: self.config.dataflow_iterations(),
                    observed: rounds,
                    context: format!(
                        "stopped function-pointer dataflow before convergence in `{}`{}",
                        self.path(instance.def_id()),
                        if self.config.max_dataflow_iterations.is_none() {
                            " (inherited from max-iterations)"
                        } else {
                            ""
                        }
                    ),
                });
                break;
            }
        }
        self.statistics.dataflow_iterations = self.statistics.dataflow_iterations.max(rounds);
        self.changed |= self.nodes[id].output.join(&locals[0], &self.policy);
        if locals.iter().any(Value::is_truncated) || self.nodes[id].output.is_truncated() {
            self.aggregate_exhaustion();
        }
        self.changed |= previous_edges != self.nodes[id].edges;
        self.changed |= previous_discovered != self.nodes[id].discovered;
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
                self.tcx.dcx().span_err(
                    span,
                    format!("insulator `{path}` has an out-of-range callback argument"),
                );
                return Value::default();
            }
            for (index, value) in values.iter().enumerate() {
                for &callback in &value.root.targets {
                    if insulated.contains(&index) {
                        // Discover async blocks constructed on the blocking
                        // thread without propagating its synchronous effects.
                        self.discover(caller, callback, Vec::new());
                    } else {
                        self.edge(caller, callback, Vec::new(), span);
                    }
                }
                if let ty::Closure(def_id, args) = *types[index].peel_refs().kind() {
                    let callback = Instance::new_raw(def_id, args);
                    if insulated.contains(&index) {
                        self.discover(caller, callback, vec![value.clone()]);
                    } else {
                        self.edge(caller, callback, vec![value.clone()], span);
                    }
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

    fn report(&self, owner: LocalDefId, roots: &[usize], async_owners: &[LocalDefId]) {
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
        let mut exhausted = self.exhausted.clone();
        while let Some(id) = queue.pop_front() {
            let node = &self.nodes[id];
            if let Some(local) = node.instance.def_id().as_local()
                && local != owner
                && async_owners.contains(&local)
                && node.prohibited.is_none()
            {
                // A separately checked async body owns its own diagnostics.
                // External async bodies are still traversed from local roots.
                continue;
            }
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
            for reason in &node.exhausted {
                if !exhausted.contains(reason) {
                    exhausted.push(reason.clone());
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
        if !exhausted.is_empty() || !incomplete.is_empty() {
            self.tcx.emit_node_span_lint(
                ASYNC_HYGIENE_INCOMPLETE,
                hir_id,
                root_span,
                DiagDecorator(|diag| {
                    diag.primary_message(
                        "async hygiene analysis is incomplete for this async context",
                    );
                    // Always identify each exhausted budget. Further occurrences
                    // share the note allowance with unsupported-call details.
                    let mut helped = HashSet::new();
                    let mut details = Vec::new();
                    for reason in &exhausted {
                        if helped.insert(reason.key) {
                            diag.note(reason.message());
                            diag.help(format!("raise async_hygiene.{} or set it to \"unlimited\"", reason.key));
                        } else {
                            details.push(reason.message());
                        }
                    }
                    details.extend(incomplete);
                    let shown = match self.config.max_incomplete_notes {
                        Limit::Finite(limit) => details.len().min(limit.saturating_sub(helped.len())),
                        Limit::Unlimited => details.len(),
                    };
                    for message in &details[..shown] {
                        diag.note(message.clone());
                    }
                    if details.len() > shown {
                        diag.note(format!("{} additional incomplete reasons omitted; raise async_hygiene.max-incomplete-notes or set it to \"unlimited\"", details.len() - shown));
                    }
                }),
            );
        }
    }
}
