//! Code shared by trait and projection goals for candidate assembly.

use super::infcx_ext::InferCtxtExt;
use super::{CanonicalResponse, Certainty, EvalCtxt, Goal};
use rustc_hir::def_id::DefId;
use rustc_infer::traits::query::NoSolution;
use rustc_middle::ty::TypeFoldable;
use rustc_middle::ty::{self, Ty, TyCtxt};
use std::fmt::Debug;

/// A candidate is a possible way to prove a goal.
///
/// It consists of both the `source`, which describes how that goal would be proven,
/// and the `result` when using the given `source`.
#[derive(Debug, Clone)]
pub(super) struct Candidate<'tcx> {
    pub(super) source: CandidateSource,
    pub(super) result: CanonicalResponse<'tcx>,
}

/// Possible ways the given goal can be proven.
#[derive(Debug, Clone, Copy)]
pub(super) enum CandidateSource {
    /// A user written impl.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// fn main() {
    ///     let x: Vec<u32> = Vec::new();
    ///     // This uses the impl from the standard library to prove `Vec<T>: Clone`.
    ///     let y = x.clone();
    /// }
    /// ```
    Impl(DefId),
    /// A builtin impl generated by the compiler. When adding a new special
    /// trait, try to use actual impls whenever possible. Builtin impls should
    /// only be used in cases where the impl cannot be manually be written.
    ///
    /// Notable examples are auto traits, `Sized`, and `DiscriminantKind`.
    /// For a list of all traits with builtin impls, check out the
    /// [`EvalCtxt::assemble_builtin_impl_candidates`] method. Not
    BuiltinImpl,
    /// An assumption from the environment.
    ///
    /// More precicely we've used the `n-th` assumption in the `param_env`.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// fn is_clone<T: Clone>(x: T) -> (T, T) {
    ///     // This uses the assumption `T: Clone` from the `where`-bounds
    ///     // to prove `T: Clone`.
    ///     (x.clone(), x)
    /// }
    /// ```
    ParamEnv(usize),
    /// If the self type is an alias type, e.g. an opaque type or a projection,
    /// we know the bounds on that alias to hold even without knowing its concrete
    /// underlying type.
    ///
    /// More precisely this candidate is using the `n-th` bound in the `item_bounds` of
    /// the self type.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// trait Trait {
    ///     type Assoc: Clone;
    /// }
    ///
    /// fn foo<T: Trait>(x: <T as Trait>::Assoc) {
    ///     // We prove `<T as Trait>::Assoc` by looking at the bounds on `Assoc` in
    ///     // in the trait definition.
    ///     let _y = x.clone();
    /// }
    /// ```
    AliasBound(usize),
}

pub(super) trait GoalKind<'tcx>: TypeFoldable<'tcx> + Copy {
    fn self_ty(self) -> Ty<'tcx>;

    fn with_self_ty(self, tcx: TyCtxt<'tcx>, self_ty: Ty<'tcx>) -> Self;

    fn trait_def_id(self, tcx: TyCtxt<'tcx>) -> DefId;

    fn consider_impl_candidate(
        ecx: &mut EvalCtxt<'_, 'tcx>,
        goal: Goal<'tcx, Self>,
        impl_def_id: DefId,
    ) -> Result<Certainty, NoSolution>;

    fn consider_builtin_sized_candidate(
        ecx: &mut EvalCtxt<'_, 'tcx>,
        goal: Goal<'tcx, Self>,
    ) -> Result<Certainty, NoSolution>;

    fn consider_assumption(
        ecx: &mut EvalCtxt<'_, 'tcx>,
        goal: Goal<'tcx, Self>,
        assumption: ty::Predicate<'tcx>,
    ) -> Result<Certainty, NoSolution>;
}
impl<'tcx> EvalCtxt<'_, 'tcx> {
    pub(super) fn assemble_and_evaluate_candidates<G: GoalKind<'tcx>>(
        &mut self,
        goal: Goal<'tcx, G>,
    ) -> Vec<Candidate<'tcx>> {
        let mut candidates = Vec::new();

        self.assemble_candidates_after_normalizing_self_ty(goal, &mut candidates);

        self.assemble_impl_candidates(goal, &mut candidates);

        self.assemble_builtin_impl_candidates(goal, &mut candidates);

        self.assemble_param_env_candidates(goal, &mut candidates);

        self.assemble_alias_bound_candidates(goal, &mut candidates);

        candidates
    }

    /// If the self type of a goal is a projection, computing the relevant candidates is difficult.
    ///
    /// To deal with this, we first try to normalize the self type and add the candidates for the normalized
    /// self type to the list of candidates in case that succeeds. Note that we can't just eagerly return in
    /// this case as projections as self types add `
    fn assemble_candidates_after_normalizing_self_ty<G: GoalKind<'tcx>>(
        &mut self,
        goal: Goal<'tcx, G>,
        candidates: &mut Vec<Candidate<'tcx>>,
    ) {
        let tcx = self.tcx();
        // FIXME: We also have to normalize opaque types, not sure where to best fit that in.
        let &ty::Alias(ty::Projection, projection_ty) = goal.predicate.self_ty().kind() else {
            return
        };
        self.infcx.probe(|_| {
            let normalized_ty = self.infcx.next_ty_infer();
            let normalizes_to_goal = goal.with(
                tcx,
                ty::Binder::dummy(ty::ProjectionPredicate {
                    projection_ty,
                    term: normalized_ty.into(),
                }),
            );
            let normalization_certainty = match self.evaluate_goal(normalizes_to_goal) {
                Ok((_, certainty)) => certainty,
                Err(NoSolution) => return,
            };

            // NOTE: Alternatively we could call `evaluate_goal` here and only have a `Normalized` candidate.
            // This doesn't work as long as we use `CandidateSource` in winnowing.
            let goal = goal.with(tcx, goal.predicate.with_self_ty(tcx, normalized_ty));
            // FIXME: This is broken if we care about the `usize` of `AliasBound` because the self type
            // could be normalized to yet another projection with different item bounds.
            let normalized_candidates = self.assemble_and_evaluate_candidates(goal);
            for mut normalized_candidate in normalized_candidates {
                normalized_candidate.result =
                    normalized_candidate.result.unchecked_map(|mut response| {
                        // FIXME: This currently hides overflow in the normalization step of the self type
                        // which is probably wrong. Maybe `unify_and` should actually keep overflow as
                        // we treat it as non-fatal anyways.
                        response.certainty = response.certainty.unify_and(normalization_certainty);
                        response
                    });
                candidates.push(normalized_candidate);
            }
        })
    }

    fn assemble_impl_candidates<G: GoalKind<'tcx>>(
        &mut self,
        goal: Goal<'tcx, G>,
        candidates: &mut Vec<Candidate<'tcx>>,
    ) {
        let tcx = self.tcx();
        tcx.for_each_relevant_impl(
            goal.predicate.trait_def_id(tcx),
            goal.predicate.self_ty(),
            |impl_def_id| match G::consider_impl_candidate(self, goal, impl_def_id)
                .and_then(|certainty| self.make_canonical_response(certainty))
            {
                Ok(result) => candidates
                    .push(Candidate { source: CandidateSource::Impl(impl_def_id), result }),
                Err(NoSolution) => (),
            },
        );
    }

    fn assemble_builtin_impl_candidates<G: GoalKind<'tcx>>(
        &mut self,
        goal: Goal<'tcx, G>,
        candidates: &mut Vec<Candidate<'tcx>>,
    ) {
        let lang_items = self.tcx().lang_items();
        let trait_def_id = goal.predicate.trait_def_id(self.tcx());
        let result = if lang_items.sized_trait() == Some(trait_def_id) {
            G::consider_builtin_sized_candidate(self, goal)
        } else {
            Err(NoSolution)
        };

        match result.and_then(|certainty| self.make_canonical_response(certainty)) {
            Ok(result) => {
                candidates.push(Candidate { source: CandidateSource::BuiltinImpl, result })
            }
            Err(NoSolution) => (),
        }
    }

    fn assemble_param_env_candidates<G: GoalKind<'tcx>>(
        &mut self,
        goal: Goal<'tcx, G>,
        candidates: &mut Vec<Candidate<'tcx>>,
    ) {
        for (i, assumption) in goal.param_env.caller_bounds().iter().enumerate() {
            match G::consider_assumption(self, goal, assumption)
                .and_then(|certainty| self.make_canonical_response(certainty))
            {
                Ok(result) => {
                    candidates.push(Candidate { source: CandidateSource::ParamEnv(i), result })
                }
                Err(NoSolution) => (),
            }
        }
    }

    fn assemble_alias_bound_candidates<G: GoalKind<'tcx>>(
        &mut self,
        goal: Goal<'tcx, G>,
        candidates: &mut Vec<Candidate<'tcx>>,
    ) {
        let alias_ty = match goal.predicate.self_ty().kind() {
            ty::Bool
            | ty::Char
            | ty::Int(_)
            | ty::Uint(_)
            | ty::Float(_)
            | ty::Adt(_, _)
            | ty::Foreign(_)
            | ty::Str
            | ty::Array(_, _)
            | ty::Slice(_)
            | ty::RawPtr(_)
            | ty::Ref(_, _, _)
            | ty::FnDef(_, _)
            | ty::FnPtr(_)
            | ty::Dynamic(..)
            | ty::Closure(..)
            | ty::Generator(..)
            | ty::GeneratorWitness(_)
            | ty::Never
            | ty::Tuple(_)
            | ty::Param(_)
            | ty::Placeholder(..)
            | ty::Infer(_)
            | ty::Error(_) => return,
            ty::Bound(..) => bug!("unexpected bound type: {goal:?}"),
            ty::Alias(_, alias_ty) => alias_ty,
        };

        for (i, (assumption, _)) in self
            .tcx()
            .bound_explicit_item_bounds(alias_ty.def_id)
            .subst_iter_copied(self.tcx(), alias_ty.substs)
            .enumerate()
        {
            match G::consider_assumption(self, goal, assumption)
                .and_then(|certainty| self.make_canonical_response(certainty))
            {
                Ok(result) => {
                    candidates.push(Candidate { source: CandidateSource::AliasBound(i), result })
                }
                Err(NoSolution) => (),
            }
        }
    }
}
