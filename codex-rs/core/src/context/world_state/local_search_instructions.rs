use super::PreviousSectionState;
use super::SectionTransition;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::LocalSearchInstructions;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LocalSearchInstructionsState {
    available: bool,
}

impl LocalSearchInstructionsState {
    pub(crate) fn new(available: bool) -> Self {
        Self { available }
    }
}

impl WorldStateSection for LocalSearchInstructionsState {
    const ID: &'static str = "kag_local_search";
    type Snapshot = bool;

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && LocalSearchInstructions::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> SectionTransition<Self::Snapshot> {
        let current = self.available;
        if matches!(previous, PreviousSectionState::Known(previous) if *previous == current) {
            return (None, None);
        }
        if matches!(previous, PreviousSectionState::Absent) && !current {
            return (Some(current), None);
        }

        (
            Some(current),
            Some(Box::new(LocalSearchInstructions::new(current))),
        )
    }
}


#[cfg(test)]
mod tests {
    use super::LocalSearchInstructionsState;
    use super::PreviousSectionState;
    use super::WorldStateSection;

    #[test]
    fn emits_guidance_when_tool_becomes_available() {
        let state = LocalSearchInstructionsState::new(true);
        let (snapshot, fragment) = state.render_diff(PreviousSectionState::Absent);
        assert_eq!(snapshot, Some(true));
        let fragment = fragment.expect("available guidance");
        assert!(fragment.body().contains("is available for this workspace"));
    }

    #[test]
    fn emits_fallback_guidance_when_tool_disappears() {
        let state = LocalSearchInstructionsState::new(false);
        let previous = true;
        let (snapshot, fragment) =
            state.render_diff(PreviousSectionState::Known(&previous));
        assert_eq!(snapshot, Some(false));
        let fragment = fragment.expect("fallback guidance");
        assert!(fragment.body().contains("is not available"));
    }
}
