//! ROOT hooks: `pub mod visual;` and private `visual_cursor_charge` after the
//! parent's cursor Vec, initialized None. ROOT stores VisualEditorState beside
//! the active View. Ordinary editor mutation owns its existing allocation
//! lifecycle; visual prepare independently admits the actual old Vec capacity
//! before wrap retirement, even if a prior visual charge is absent or stale.
//! Every edit,
//! cursor restore/normalization, body loan, tab or geometry change must advance
//! its relevant fence before another worker result can be prepared.
use super::{Cursor, Damage, Editor};
use crate::{
    visual_rows::{
        AccountedVec, AllocationBudget, AllocationCharge, BodyOwner, CaretAffinity, CursorSet, LayoutPage, MAX_CURSORS,
        MAX_ROWS, NavigationFence, ResolvedNavigation, VisualCursor, WrapError, WrapResult,
    },
    wrap_session::{RetiredOwner, RetirementPermit},
};
use std::{mem::size_of, ops::ControlFlow};

pub struct VisualEditorState {
    pub fence: NavigationFence,
    pub cursors: CursorSet,
    /// Prepared rows only. Replaced pages retire through a reserved worker slot.
    pub page: Option<LayoutPage>,
    _header: AllocationCharge,
}
impl VisualEditorState {
    pub fn capture(editor: &Editor, fence: NavigationFence, budget: &AllocationBudget) -> WrapResult<Self> {
        let header = budget.reserve(size_of::<Self>())?;
        let mut cursors = AccountedVec::new(budget, MAX_CURSORS, MAX_CURSORS * size_of::<VisualCursor>())?;
        cursors.reserve_len(editor.cursors.len())?;
        for cursor in &editor.cursors {
            cursors.data.push(VisualCursor {
                anchor: cursor.anchor,
                head: cursor.head,
                anchor_affinity: CaretAffinity::Downstream,
                head_affinity: CaretAffinity::Downstream,
                // A newly captured logical-column goal is NOT a visual goal.
                // Preserve visual goals only in this transient display state.
                desired_cell: None,
            });
        }
        let cursors = CursorSet { cursors, primary: editor.primary };
        let state = Self { fence, cursors, page: None, _header: header };
        if !editor.visual_identity_matches(&state, fence) {
            return Err(WrapError::Stale);
        }
        Ok(state)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareFault {
    None,
    Validation,
    Normalization,
    Reveal,
    Damage,
    Retirement,
}
pub struct PrepareFailure {
    pub navigation: ResolvedNavigation,
    pub permit: RetirementPermit,
    pub error: WrapError,
}
impl std::fmt::Debug for PrepareFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrepareFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
pub struct CommitFailure {
    pub prepared: PreparedVisualCommit,
    pub error: WrapError,
}
impl std::fmt::Debug for CommitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitFailure").field("error", &self.error).finish_non_exhaustive()
    }
}
#[must_use = "commit atomically or retain/retire this exact prepared owner"]
pub struct PreparedVisualCommit {
    navigation: Option<ResolvedNavigation>,
    permit: Option<RetirementPermit>,
    editor_cursors: Option<AccountedVec<Cursor>>,
    display_cursors: Option<CursorSet>,
    primary: usize,
    next_fence: NavigationFence,
    damage: Option<ChargedDamage>,
    retirement: Option<Box<RetiredVisualState>>,
    expected_page: Option<(crate::visual_rows::LayoutKey, crate::visual_rows::TopAnchor)>,
    expected_view: (u64, usize, Option<u64>),
    expected_cursor_capacity: usize,
    _temporary: AllocationCharge,
}
impl PreparedVisualCommit {
    /// Transfer this exact owner to its own reserved worker slot. Remove the
    /// active permit BEFORE boxing/publication: worker destruction must never
    /// drop an embedded active permit inside that permit's own retirement slot.
    /// Refusal returns all fields and the original active permit unchanged.
    pub fn retire(self) -> ControlFlow<Self, ()> {
        self.retire_before_publish(|| {})
    }
    fn retire_before_publish(mut self, before_publish: impl FnOnce()) -> ControlFlow<Self, ()> {
        let Some(permit) = self.permit.take() else {
            return ControlFlow::Break(self);
        };
        if !permit.is_current() {
            self.permit = Some(permit);
            return ControlFlow::Break(self);
        }
        // _temporary already charges the retained header. Admit stack-to-Box
        // construction overlap in that SAME budget before allocating the Box.
        let transfer = match self._temporary.reserve_transfer(size_of::<Self>()) {
            Ok(charge) => charge,
            Err(_) => {
                self.permit = Some(permit);
                return ControlFlow::Break(self);
            }
        };
        #[cfg(test)]
        self._temporary.record_transfer_allocation();
        let mut boxed = Some(Box::new(self));
        before_publish();
        match permit.publish(|| ((), boxed.take().map(RetiredOwner::PreparedCommit))) {
            Ok(()) => ControlFlow::Continue(()),
            Err(permit) => {
                let mut prepared = *boxed.take().expect("unaccepted prepared owner");
                prepared.permit = Some(permit);
                drop(transfer);
                ControlFlow::Break(prepared)
            }
        }
    }
}
pub struct PublishedVisualCursor {
    pub damage: ChargedDamage,
    pub fence: NavigationFence,
    pub request_fence: NavigationFence,
}
/// The row allocation and its admission charge have one sealed owner. Hosts
/// may borrow damage for painting or move this wrapper into their own queue;
/// they cannot detach its Vec and leave accounting behind in editor state.
pub struct ChargedDamage {
    damage: Damage,
    _charge: AllocationCharge,
}
impl ChargedDamage {
    pub fn damage(&self) -> &Damage {
        &self.damage
    }
}
/// Prepared empty, filled without allocation at publication, then destroyed on
/// the fixed worker. Cursor bytes and their charge always move together.
pub struct RetiredVisualState {
    cursors: Vec<Cursor>,
    cursor_charge: Option<AllocationCharge>,
    _adopted_cursor_charge: AllocationCharge,
    page: Option<LayoutPage>,
    _header: AllocationCharge,
}

impl Editor {
    fn visual_identity_matches(&self, state: &VisualEditorState, current: NavigationFence) -> bool {
        current == state.fence
            && current.key.body == BodyOwner::Text
            && current.key.source.source_epoch == self.document.source_epoch
            && current.key.source.revision == self.document.revision
            && self.document.representation == crate::hex_session::Representation::DecodedText
            && state.cursors.primary == self.primary
            && state.cursors.cursors().len() == self.cursors.len()
            && state
                .cursors
                .cursors()
                .iter()
                .zip(&self.cursors)
                .all(|(display, actual)| display.anchor == actual.anchor && display.head == actual.head)
    }
    fn visual_end_burst_preflight(&self) -> WrapResult<()> {
        if self.compound.is_some()
            || self.saving
            || self.has_pending_edit_receipt()
            || self.has_pending_save_completion()
        {
            return Err(WrapError::Busy);
        }
        // Existing read-only API rejects an open transaction and u64::MAX
        // mutation epoch. It does NOT advance/reserve an epoch. The Editor
        // retains these same roots while the duplicate seed is dropped here.
        drop(self.document.buffer.worker_edit_seed().map_err(WrapError::Io)?);
        Ok(())
    }
    pub fn prepare_visual_cursor_commit(
        &self,
        state: &VisualEditorState,
        navigation: ResolvedNavigation,
        current: NavigationFence,
        budget: &AllocationBudget,
        permit: RetirementPermit,
    ) -> ControlFlow<PrepareFailure, PreparedVisualCommit> {
        self.prepare_visual_cursor_commit_with_fault(state, navigation, current, budget, permit, PrepareFault::None)
    }
    /// Fault hooks reject BEFORE any Editor mutation, including end_burst.
    pub fn prepare_visual_cursor_commit_with_fault(
        &self,
        state: &VisualEditorState,
        navigation: ResolvedNavigation,
        current: NavigationFence,
        budget: &AllocationBudget,
        permit: RetirementPermit,
        fault: PrepareFault,
    ) -> ControlFlow<PrepareFailure, PreparedVisualCommit> {
        let prepare = || -> WrapResult<_> {
            fail_at(fault, PrepareFault::Validation)?;
            if !self.visual_identity_matches(state, current)
                || navigation.fence != current
                || navigation.expected.primary != self.primary
                || navigation.expected.cursors() != state.cursors.cursors()
                || navigation.reveal.key != current.key
            {
                return Err(WrapError::Stale);
            }
            self.visual_end_burst_preflight()?;
            if navigation.targets.cursors().is_empty()
                || navigation.targets.cursors().len() > MAX_CURSORS
                || navigation.targets.primary >= navigation.targets.cursors().len()
            {
                return Err(WrapError::Invalid("invalid resolved targets"));
            }
            let length = self.document.buffer.len();
            if navigation.targets.cursors().iter().any(|c| c.anchor > length || c.head > length) {
                return Err(WrapError::Invalid("resolved cursor outside document"));
            }
            let next_generation = current
                .cursor_set_generation
                .checked_add(1)
                .ok_or(WrapError::Admission("cursor generation exhausted"))?;
            let temporary = budget.reserve(size_of::<PreparedVisualCommit>() + MAX_ROWS)?;
            fail_at(fault, PrepareFault::Normalization)?;
            let (editor_cursors, display_cursors, primary) = normalize_targets(&navigation.targets, budget)?;
            fail_at(fault, PrepareFault::Reveal)?;
            let primary_cursor = display_cursors.cursors()[primary];
            if navigation.reveal.source_to_screen(primary_cursor.head, primary_cursor.head_affinity).is_none() {
                return Err(WrapError::Invalid("primary target has no exact reveal mapping"));
            }
            fail_at(fault, PrepareFault::Damage)?;
            let damage =
                prepare_damage(state.page.as_ref(), &navigation.reveal, &self.cursors, &editor_cursors, budget)?;
            fail_at(fault, PrepareFault::Retirement)?;
            if !permit.is_current() {
                return Err(WrapError::Busy);
            }
            let retirement_charge = budget.reserve(size_of::<RetiredVisualState>())?;
            let old_cursor_bytes = self
                .cursors
                .capacity()
                .checked_mul(size_of::<Cursor>())
                .ok_or(WrapError::Admission("old cursor capacity overflow"))?;
            // The old Vec belongs to ordinary Editor code before adoption. Its
            // prior visual charge can be None or conservatively stale after a
            // nonvisual edit. Admit its ACTUAL capacity, retaining the old
            // charge too; never infer coverage from that prior charge.
            let adopted_cursor_charge = budget.reserve(old_cursor_bytes)?;
            let retirement = Box::new(RetiredVisualState {
                cursors: Vec::new(),
                cursor_charge: None,
                _adopted_cursor_charge: adopted_cursor_charge,
                page: None,
                _header: retirement_charge,
            });
            Ok((
                editor_cursors,
                display_cursors,
                primary,
                NavigationFence { cursor_set_generation: next_generation, ..current },
                damage,
                retirement,
                temporary,
            ))
        };
        match prepare() {
            Ok((editor_cursors, display_cursors, primary, next_fence, damage, retirement, temporary)) => {
                ControlFlow::Continue(PreparedVisualCommit {
                    navigation: Some(navigation),
                    permit: Some(permit),
                    editor_cursors: Some(editor_cursors),
                    display_cursors: Some(display_cursors),
                    primary,
                    next_fence,
                    damage: Some(damage),
                    retirement: Some(retirement),
                    expected_page: state.page.as_ref().map(|page| (page.key, page.anchor)),
                    expected_view: (self.top, self.rows, self.box_column),
                    expected_cursor_capacity: self.cursors.capacity(),
                    _temporary: temporary,
                })
            }
            Err(error) => ControlFlow::Break(PrepareFailure { navigation, permit, error }),
        }
    }
    /// Cheap fences plus an already-admitted infallible publication. ROOT must
    /// pop its relative FIFO head only after this function returns success.
    pub fn commit_visual_cursor_update(
        &mut self,
        state: &mut VisualEditorState,
        mut prepared: PreparedVisualCommit,
        current: NavigationFence,
    ) -> ControlFlow<CommitFailure, PublishedVisualCursor> {
        let navigation = prepared.navigation.as_ref().expect("prepared navigation owner");
        if !self.visual_identity_matches(state, current)
            || navigation.fence != current
            || navigation.expected.cursors() != state.cursors.cursors()
            || prepared.expected_page != state.page.as_ref().map(|page| (page.key, page.anchor))
            || prepared.expected_view != (self.top, self.rows, self.box_column)
            || prepared.expected_cursor_capacity != self.cursors.capacity()
        {
            return ControlFlow::Break(CommitFailure { prepared, error: WrapError::Stale });
        }
        if let Err(error) = self.visual_end_burst_preflight() {
            return ControlFlow::Break(CommitFailure { prepared, error });
        }
        let permit = prepared.permit.take().expect("prepared retirement permit");
        // The reservation lock prevents stop/admission-close racing with the
        // publication. No source access, allocation, normalization or lookup
        // remains inside this closure. Exclusive &mut Editor prevents a core
        // epoch change between the read-only preflight and end_burst.
        match permit.publish(|| {
            let (cursors, charge) = prepared.editor_cursors.take().expect("prepared cursors").into_parts();
            let display = prepared.display_cursors.take().expect("prepared display cursors");
            let navigation = prepared.navigation.take().expect("prepared navigation");
            self.end_burst();
            let old_cursors = std::mem::replace(&mut self.cursors, cursors);
            // ROOT parent hook: private field immediately after `cursors`:
            // visual_cursor_charge: Option<AllocationCharge>, initially None.
            let old_cursor_charge = self.visual_cursor_charge.replace(charge);
            self.primary = prepared.primary;
            self.box_column = None;
            // Keep parent top logical. The exact visual anchor lives in page.
            self.top = navigation.reveal.rows().first().map_or(self.top, |row| row.logical_line);
            self.rows = usize::from(current.key.geometry.rows);
            state.cursors = display;
            state.fence = prepared.next_fence;
            let old_page = state.page.replace(navigation.reveal);
            let mut retired = prepared.retirement.take().expect("prepared retirement owner");
            retired.cursors = old_cursors;
            retired.cursor_charge = old_cursor_charge;
            retired.page = old_page;
            let published = PublishedVisualCursor {
                damage: prepared.damage.take().expect("prepared damage"),
                fence: prepared.next_fence,
                request_fence: current,
            };
            (published, Some(RetiredOwner::VisualState(retired)))
        }) {
            Ok(published) => ControlFlow::Continue(published),
            Err(permit) => {
                prepared.permit = Some(permit);
                ControlFlow::Break(CommitFailure { prepared, error: WrapError::Closed })
            }
        }
    }
}

fn fail_at(actual: PrepareFault, phase: PrepareFault) -> WrapResult<()> {
    if actual == phase { Err(WrapError::Admission("injected visual prepare failure")) } else { Ok(()) }
}

fn normalize_targets(
    targets: &CursorSet,
    budget: &AllocationBudget,
) -> WrapResult<(AccountedVec<Cursor>, CursorSet, usize)> {
    let display = targets.normalized(budget)?;
    let mut actual = AccountedVec::new(budget, MAX_CURSORS, MAX_CURSORS * size_of::<Cursor>())?;
    actual.reserve_len(display.cursors().len())?;
    actual.data.extend(display.cursors().iter().map(|c| Cursor {
        anchor: c.anchor,
        head: c.head,
        // Keep the parent's logical-column field out of the visual row space.
        desired_column: None,
    }));
    let primary = display.primary;
    Ok((actual, display, primary))
}

fn mark_cursor_damage(page: &LayoutPage, cursors: &[Cursor], rows: &mut [bool], scroll: i64) {
    for cursor in cursors {
        let range = cursor.selection();
        let first = page.rows().partition_point(|row| row.source_coverage.end < range.start);
        let last = page.rows().partition_point(|row| row.source_coverage.start <= range.end);
        let length = rows.len() as i64;
        let first = (first as i64 - scroll).clamp(0, length) as usize;
        let last = (last as i64 - scroll).clamp(0, length) as usize;
        rows[first..last].fill(true);
    }
}
fn prepare_damage(
    old: Option<&LayoutPage>,
    new: &LayoutPage,
    old_cursors: &[Cursor],
    new_cursors: &[Cursor],
    budget: &AllocationBudget,
) -> WrapResult<ChargedDamage> {
    let length = usize::from(new.key.geometry.rows);
    let mut changed = [false; MAX_ROWS];
    let delta = new.delta_from(old);
    let mut damage = Damage { full: delta.full, scroll: delta.scroll, status: true, ..Damage::default() };
    for row in delta.changed_rows() {
        changed[row] = true;
    }
    if let Some(old) = old {
        mark_cursor_damage(old, old_cursors, &mut changed[..length], damage.scroll);
    }
    mark_cursor_damage(new, new_cursors, &mut changed[..length], 0);
    let mut rows = AccountedVec::new(budget, MAX_ROWS, MAX_ROWS * size_of::<usize>())?;
    rows.reserve_len(length)?;
    rows.data.extend(changed[..length].iter().enumerate().filter_map(|(row, changed)| changed.then_some(row)));
    let (rows, charge) = rows.into_parts();
    damage.rows = rows;
    Ok(ChargedDamage { damage, _charge: charge })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Document, visual_rows::*, wrap_session::WrapSession};
    use std::{sync::Arc, time::Duration};

    trait OutcomeTestExt<B, C> {
        fn unwrap(self) -> C;
    }
    impl<B: std::fmt::Debug, C> OutcomeTestExt<B, C> for ControlFlow<B, C> {
        fn unwrap(self) -> C {
            match self {
                ControlFlow::Continue(value) => value,
                ControlFlow::Break(owner) => panic!("unexpected refusal: {owner:?}"),
            }
        }
    }

    fn setup() -> (Editor, VisualEditorState, AllocationBudget, Arc<dyn WrapSource>, LayoutEngine) {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("a\nb\nc", Duration::ZERO).unwrap();
        editor.box_column = Some(1);
        let budget = AllocationBudget::default();
        let key = LayoutKey {
            source: SourceIdentity {
                document_id: 7,
                source_epoch: editor.document.source_epoch,
                incarnation: 13,
                revision: editor.document.revision,
            },
            input_epoch: 19,
            layout_generation: 23,
            body_generation: 29,
            body: BodyOwner::Text,
            geometry: LayoutGeometry {
                columns: 4,
                rows: 2,
                tab_size: 4,
                font_generation: 31,
                width_policy_generation: 37,
                settings_generation: 41,
            },
        };
        let fence = NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 };
        let mut state = VisualEditorState::capture(&editor, fence, &budget).unwrap();
        let source: Arc<dyn WrapSource> = Arc::new(SnapshotSource::new(key.source, editor.document.snapshot()));
        let mut engine = LayoutEngine::new(budget.clone()).unwrap();
        state.page = Some(engine.fill(key, source.as_ref(), TopAnchor::default(), &NoCancellation).unwrap());
        (editor, state, budget, source, engine)
    }
    fn navigation(
        engine: &mut LayoutEngine,
        source: &dyn WrapSource,
        state: &VisualEditorState,
        budget: &AllocationBudget,
    ) -> ResolvedNavigation {
        engine
            .navigate(
                state.fence,
                source,
                CursorSet::new(budget, state.cursors.cursors(), state.cursors.primary).unwrap(),
                NavigationCommand { motion: VisualMotion::Up, count: 1, extend: false },
                &NoCancellation,
            )
            .unwrap()
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Fingerprint {
        cursors: Vec<Cursor>,
        primary: usize,
        top: u64,
        rows: usize,
        box_column: Option<u64>,
        burst: Option<(Duration, Vec<Cursor>)>,
        undo: usize,
        redo: usize,
        history: (usize, usize),
        revision: u64,
        saved_revision: u64,
        bytes: Vec<u8>,
        display: Vec<VisualCursor>,
        display_fence: NavigationFence,
        page_anchor: Option<TopAnchor>,
        cursor_charge: bool,
        cursor_capacity: usize,
    }
    fn fingerprint(editor: &Editor, state: &VisualEditorState) -> Fingerprint {
        let mut bytes = Vec::new();
        editor.document.snapshot().write_to(&mut bytes).unwrap();
        Fingerprint {
            cursors: editor.cursors.clone(),
            primary: editor.primary,
            top: editor.top,
            rows: editor.rows,
            box_column: editor.box_column,
            burst: editor.burst.clone(),
            undo: editor.undo.len(),
            redo: editor.redo.len(),
            history: editor.document.buffer.history_depth(),
            revision: editor.document.revision,
            saved_revision: editor.document.saved_revision,
            bytes,
            display: state.cursors.cursors().to_vec(),
            display_fence: state.fence,
            page_anchor: state.page.as_ref().map(|page| page.anchor),
            cursor_charge: editor.visual_cursor_charge.is_some(),
            cursor_capacity: editor.cursors.capacity(),
        }
    }
    fn settle(ticket: crate::wrap_session::SettlementTicket) {
        ticket.settle(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn wrap_atomic_prepare_faults_preserve_cursor_box_view_burst_undo_and_source() {
        for phase in [
            PrepareFault::Validation,
            PrepareFault::Normalization,
            PrepareFault::Reveal,
            PrepareFault::Damage,
            PrepareFault::Retirement,
        ] {
            let (editor, state, budget, source, mut engine) = setup();
            let before = fingerprint(&editor, &state);
            assert!(before.burst.is_some());
            let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
            let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
            let result = editor.prepare_visual_cursor_commit_with_fault(
                &state,
                nav,
                state.fence,
                &budget,
                session.reserve_retirement().unwrap(),
                phase,
            );
            let failure = match result {
                ControlFlow::Break(failure) => failure,
                ControlFlow::Continue(_) => panic!("fault must reject prepare"),
            };
            assert_eq!(fingerprint(&editor, &state), before, "{phase:?}");
            assert_eq!(failure.navigation.expected.cursors(), state.cursors.cursors());
            drop(failure);
            settle(session.stop());
        }
    }

    #[test]
    fn wrap_atomic_publication_rechecks_fences_and_ends_burst_only_on_success() {
        let (mut editor, mut state, budget, source, mut engine) = setup();
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let before = fingerprint(&editor, &state);
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        let current = state.fence;
        let prepared = editor
            .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
            .unwrap();
        assert_eq!(fingerprint(&editor, &state), before);
        let stale = NavigationFence { intent_generation: current.intent_generation + 1, ..current };
        let failure = match editor.commit_visual_cursor_update(&mut state, prepared, stale) {
            ControlFlow::Break(failure) => failure,
            ControlFlow::Continue(_) => panic!("stale commit must refuse"),
        };
        assert_eq!(fingerprint(&editor, &state), before);
        let published = editor.commit_visual_cursor_update(&mut state, failure.prepared, current).unwrap();
        assert_eq!(published.request_fence, current);
        assert_eq!(published.fence.cursor_set_generation, current.cursor_set_generation + 1);
        assert!(editor.burst.is_none());
        assert_eq!(editor.box_column, None);
        assert_eq!(editor.primary_cursor().head, 3);
        assert_eq!(editor.document.revision, before.revision);
        assert_eq!(editor.undo.len(), before.undo);
        assert_eq!(editor.document.buffer.history_depth(), before.history);
        settle(session.stop());
    }

    #[test]
    fn wrap_atomic_parent_change_and_open_compound_refuse_before_end_burst() {
        let (mut editor, state, budget, source, mut engine) = setup();
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        editor.document.buffer.begin();
        let before = fingerprint(&editor, &state);
        let result = editor.prepare_visual_cursor_commit(
            &state,
            nav,
            state.fence,
            &budget,
            session.reserve_retirement().unwrap(),
        );
        let failure = match result {
            ControlFlow::Break(failure) => failure,
            ControlFlow::Continue(_) => panic!("open compound must refuse"),
        };
        assert_eq!(fingerprint(&editor, &state), before);
        drop(failure);
        editor.document.buffer.cancel();
        settle(session.stop());
    }

    #[test]
    fn wrap_atomic_4096_cursors_are_bounded_and_never_partially_published() {
        let mut editor = Editor::new(Document::new());
        editor.document.buffer.insert(0, &vec![b'x'; MAX_CURSORS + 2]).unwrap();
        editor.cursors = (0..MAX_CURSORS as u64).map(Cursor::caret).collect();
        editor.primary = MAX_CURSORS - 1;
        let budget = AllocationBudget::default();
        let key = LayoutKey {
            source: SourceIdentity {
                document_id: 1,
                source_epoch: editor.document.source_epoch,
                incarnation: 1,
                revision: editor.document.revision,
            },
            geometry: LayoutGeometry { columns: 4, rows: 1, tab_size: 4, ..LayoutGeometry::default() },
            ..LayoutKey::default()
        };
        let current = NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 };
        let state = VisualEditorState::capture(&editor, current, &budget).unwrap();
        let source = SnapshotSource::new(key.source, editor.document.snapshot());
        let mut engine = LayoutEngine::new(budget.clone()).unwrap();
        let targets = CursorSet::new(&budget, state.cursors.cursors(), state.cursors.primary).unwrap();
        let reveal = engine
            .fill(
                key,
                &source,
                TopAnchor { source_byte: editor.primary_cursor().head, ..TopAnchor::default() },
                &NoCancellation,
            )
            .unwrap();
        let nav = ResolvedNavigation {
            fence: current,
            expected: CursorSet::new(&budget, state.cursors.cursors(), state.cursors.primary).unwrap(),
            targets,
            reveal,
            _header: budget.reserve(size_of::<ResolvedNavigation>()).unwrap(),
        };
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let before = fingerprint(&editor, &state);
        let result = editor.prepare_visual_cursor_commit_with_fault(
            &state,
            nav,
            current,
            &budget,
            session.reserve_retirement().unwrap(),
            PrepareFault::Retirement,
        );
        let failure = match result {
            ControlFlow::Break(failure) => failure,
            ControlFlow::Continue(_) => panic!("late precommit fault must refuse"),
        };
        assert_eq!(fingerprint(&editor, &state), before);
        assert!(budget.peak_bytes() <= MAX_SESSION_BYTES);
        drop(failure);
        settle(session.stop());
    }

    #[test]
    fn wrap_normal_interior_scalar_cursor_resolves_and_commits_without_changing_source_bytes() {
        let mut editor = Editor::new(Document::new());
        editor.insert_text("e\u{301}\nX", Duration::ZERO).unwrap();
        editor.set_cursors(vec![Cursor::caret(1)], 0).unwrap();
        let budget = AllocationBudget::default();
        let key = LayoutKey {
            source: SourceIdentity {
                document_id: 1,
                source_epoch: editor.document.source_epoch,
                incarnation: 1,
                revision: editor.document.revision,
            },
            geometry: LayoutGeometry { columns: 4, rows: 2, tab_size: 4, ..LayoutGeometry::default() },
            ..LayoutKey::default()
        };
        let current = NavigationFence { key, cursor_set_generation: 1, intent_generation: 1 };
        let mut state = VisualEditorState::capture(&editor, current, &budget).unwrap();
        let source = SnapshotSource::new(key.source, editor.document.snapshot());
        let mut engine = LayoutEngine::new(budget.clone()).unwrap();
        state.page =
            Some(engine.fill(key, &source, crate::visual_rows::TopAnchor::default(), &NoCancellation).unwrap());
        let nav = engine
            .navigate(
                current,
                &source,
                CursorSet::new(&budget, state.cursors.cursors(), 0).unwrap(),
                NavigationCommand { motion: VisualMotion::Down, count: 1, extend: true },
                &NoCancellation,
            )
            .unwrap();
        assert_eq!(nav.expected.cursors()[0].head, 1);
        assert_eq!(editor.primary_cursor().head, 1);
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let before = fingerprint(&editor, &state);
        let prepared = editor
            .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
            .unwrap();
        assert_eq!(fingerprint(&editor, &state), before);
        editor.commit_visual_cursor_update(&mut state, prepared, current).unwrap();
        assert_eq!(editor.primary_cursor().anchor, 1);
        assert_eq!(editor.primary_cursor().head, 4);
        assert_eq!(editor.primary_cursor().desired_column, None, "visual goals never occupy the logical column field");
        assert_eq!(state.cursors.cursors()[0].desired_cell, Some(0));
        let after = fingerprint(&editor, &state);
        assert_eq!(after.bytes, before.bytes);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.undo, before.undo);
        settle(session.stop());
    }

    #[test]
    fn wrap_damage_publications_retain_independent_charges_after_second_commit_and_state_drop() {
        let (mut editor, mut state, budget, source, mut engine) = setup();
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        let current = state.fence;
        let prepared = editor
            .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
            .unwrap();
        let first = editor.commit_visual_cursor_update(&mut state, prepared, current).unwrap();
        let first_bytes = first.damage.damage().rows.capacity() * size_of::<usize>();
        assert!(first_bytes > 0);
        let first_pointer = first.damage.damage().rows.as_ptr();
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        let current = state.fence;
        let prepared = editor
            .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
            .unwrap();
        let second = editor.commit_visual_cursor_update(&mut state, prepared, current).unwrap();
        let second_bytes = second.damage.damage().rows.capacity() * size_of::<usize>();
        assert!(second_bytes > 0);
        assert_eq!(first.damage.damage().rows.as_ptr(), first_pointer);
        let cursor_bytes = editor.cursors.capacity() * size_of::<Cursor>();
        assert!(cursor_bytes > 0);
        assert!(editor.visual_cursor_charge.is_some());
        drop(state);
        drop(engine);
        drop(source);
        settle(session.stop());
        let metadata = AllocationBudget::new(0).retained_bytes();
        assert_eq!(
            budget.retained_bytes(),
            metadata + cursor_bytes + first_bytes + second_bytes,
            "live Editor cursors and both real damage owners stay charged after display state destruction"
        );
        assert!(!first.damage.damage().rows.is_empty());
        assert!(!second.damage.damage().rows.is_empty());
        assert!(!editor.cursors.is_empty());
        assert!(editor.visual_cursor_charge.is_some());
        drop(first);
        assert_eq!(budget.retained_bytes(), metadata + cursor_bytes + second_bytes);
        drop(editor);
        assert_eq!(budget.retained_bytes(), metadata + second_bytes);
        drop(second);
        assert_eq!(budget.retained_bytes(), metadata);
    }

    #[test]
    fn wrap_real_prepare_allocation_refusal_returns_exact_navigation_and_permit_for_reuse() {
        let (mut editor, mut state, budget, source, mut engine) = setup();
        let (wake, ready) = std::sync::mpsc::sync_channel(1);
        let mut session = WrapSession::with_budget(budget.clone(), move || {
            let _ = wake.try_send(());
        })
        .unwrap();
        // Admit/start the one worker before deliberately exhausting the shared
        // budget, so the refusal is prepare admission, not worker-startup luck.
        let current = state.fence;
        assert!(matches!(
            session.submit(crate::wrap_session::WrapRequest {
                fence: current,
                source: source.clone(),
                anchor: TopAnchor::default(),
                intent: crate::wrap_session::LayoutIntent::Fill
            }),
            ControlFlow::Continue(())
        ));
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let startup = session.take_update(current).expect("ready startup page");
        assert!(startup.result.is_ok());
        drop(startup);
        let permit = session.reserve_retirement().unwrap();
        let other = session.reserve_retirement().unwrap();
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        let targets = nav.targets.cursors().as_ptr();
        let reveal = nav.reveal.rows().as_ptr();
        let before = fingerprint(&editor, &state);
        let occupied = budget.reserve(MAX_SESSION_BYTES - budget.retained_bytes()).unwrap();
        let failure = match editor.prepare_visual_cursor_commit(&state, nav, current, &budget, permit) {
            ControlFlow::Break(failure) => failure,
            ControlFlow::Continue(_) => panic!("real exhausted budget must refuse prepare"),
        };
        assert!(matches!(failure.error, WrapError::Admission(_)));
        assert_eq!(fingerprint(&editor, &state), before);
        assert_eq!(failure.navigation.targets.cursors().as_ptr(), targets);
        assert_eq!(failure.navigation.reveal.rows().as_ptr(), reveal);
        assert!(failure.permit.is_current());
        assert!(matches!(session.reserve_retirement(), Err(WrapError::Busy)));
        drop(occupied);
        let prepared =
            editor.prepare_visual_cursor_commit(&state, failure.navigation, current, &budget, failure.permit).unwrap();
        assert_eq!(fingerprint(&editor, &state), before);
        editor.commit_visual_cursor_update(&mut state, prepared, current).unwrap();
        assert!(editor.burst.is_none());
        assert_eq!(editor.primary_cursor().head, 3);
        drop(other);
        settle(session.stop());
    }

    #[test]
    fn wrap_real_reveal_refusal_preserves_burst_then_reuses_exact_returned_owners() {
        let (mut editor, mut state, budget, source, mut engine) = setup();
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let mut nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        // A valid page with the exact key, but the wrong offscreen anchor:
        // target head 3 is outside the final source row at bytes 4..5.
        nav.reveal = engine
            .fill(
                state.fence.key,
                source.as_ref(),
                TopAnchor { source_byte: 4, ..TopAnchor::default() },
                &NoCancellation,
            )
            .unwrap();
        let targets = nav.targets.cursors().as_ptr();
        let reveal = nav.reveal.rows().as_ptr();
        let before = fingerprint(&editor, &state);
        let current = state.fence;
        let failure = match editor.prepare_visual_cursor_commit(
            &state,
            nav,
            current,
            &budget,
            session.reserve_retirement().unwrap(),
        ) {
            ControlFlow::Break(failure) => failure,
            ControlFlow::Continue(_) => panic!("unmapped real reveal must refuse"),
        };
        assert!(matches!(failure.error, WrapError::Invalid("primary target has no exact reveal mapping")));
        assert_eq!(fingerprint(&editor, &state), before);
        assert_eq!(failure.navigation.targets.cursors().as_ptr(), targets);
        assert_eq!(failure.navigation.reveal.rows().as_ptr(), reveal);
        assert!(failure.permit.is_current());
        let failure =
            match editor.prepare_visual_cursor_commit(&state, failure.navigation, current, &budget, failure.permit) {
                ControlFlow::Break(failure) => failure,
                ControlFlow::Continue(_) => {
                    panic!("same refused navigation must still refuse until reveal is repaired")
                }
            };
        assert!(matches!(failure.error, WrapError::Invalid("primary target has no exact reveal mapping")));
        assert_eq!(fingerprint(&editor, &state), before);
        assert_eq!(failure.navigation.targets.cursors().as_ptr(), targets);
        assert_eq!(failure.navigation.reveal.rows().as_ptr(), reveal);
        assert!(failure.permit.is_current());
        let mut nav = failure.navigation;
        nav.reveal = engine
            .fill(current.key, source.as_ref(), TopAnchor { source_byte: 2, ..TopAnchor::default() }, &NoCancellation)
            .unwrap();
        assert_eq!(
            nav.targets.cursors().as_ptr(),
            targets,
            "reuse returned navigation rather than recomputing cursors"
        );
        let prepared = editor.prepare_visual_cursor_commit(&state, nav, current, &budget, failure.permit).unwrap();
        assert_eq!(fingerprint(&editor, &state), before);
        editor.commit_visual_cursor_update(&mut state, prepared, current).unwrap();
        assert_eq!(editor.primary_cursor().head, 3);
        settle(session.stop());
    }

    #[test]
    fn wrap_cursor_retirement_adopts_actual_capacity_with_absent_or_stale_prior_charge() {
        for stale in [false, true] {
            let (mut editor, mut state, budget, source, mut engine) = setup();
            // Model ordinary Editor Vec growth outside the wrap allocation
            // domain. A previous visual charge cannot describe this capacity.
            editor.cursors.try_reserve_exact(32).unwrap();
            let old_bytes = editor.cursors.capacity() * size_of::<Cursor>();
            if stale {
                editor.visual_cursor_charge = Some(budget.reserve(size_of::<Cursor>()).unwrap());
                assert!(editor.visual_cursor_charge.as_ref().unwrap().charged_bytes() < old_bytes);
            } else {
                assert!(editor.visual_cursor_charge.is_none());
            }
            let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
            let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
            let current = state.fence;
            let before = fingerprint(&editor, &state);
            let prepared = editor
                .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
                .unwrap();
            assert_eq!(fingerprint(&editor, &state), before);
            assert_eq!(prepared.retirement.as_ref().unwrap()._adopted_cursor_charge.charged_bytes(), old_bytes);
            // Same cursor bytes/identities, different allocation: final cheap
            // fences must reject rather than undercharge the retirement owner.
            editor.cursors.try_reserve_exact(editor.cursors.capacity() + 1).unwrap();
            let changed = fingerprint(&editor, &state);
            let failure = match editor.commit_visual_cursor_update(&mut state, prepared, current) {
                ControlFlow::Break(failure) => failure,
                ControlFlow::Continue(_) => panic!("capacity change bypassed final adoption fence"),
            };
            assert!(matches!(failure.error, WrapError::Stale));
            assert_eq!(fingerprint(&editor, &state), changed);
            drop(failure);
            let old_bytes = editor.cursors.capacity() * size_of::<Cursor>();
            let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
            let prepared = editor
                .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
                .unwrap();
            assert_eq!(prepared.retirement.as_ref().unwrap()._adopted_cursor_charge.charged_bytes(), old_bytes);
            editor.commit_visual_cursor_update(&mut state, prepared, current).unwrap();
            assert_eq!(
                editor.visual_cursor_charge.as_ref().unwrap().charged_bytes(),
                editor.cursors.capacity() * size_of::<Cursor>()
            );
            settle(session.stop());
        }
    }

    #[test]
    fn wrap_prepared_retirement_refuses_before_box_allocation_then_uses_its_own_slot() {
        let (editor, state, budget, source, mut engine) = setup();
        let (wake, ready) = std::sync::mpsc::sync_channel(1);
        let mut session = WrapSession::with_budget(budget.clone(), move || {
            let _ = wake.try_send(());
        })
        .unwrap();
        let current = state.fence;
        assert!(matches!(
            session.submit(crate::wrap_session::WrapRequest {
                fence: current,
                source: source.clone(),
                anchor: TopAnchor::default(),
                intent: crate::wrap_session::LayoutIntent::Fill
            }),
            ControlFlow::Continue(())
        ));
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let startup = session.take_update(current).unwrap();
        assert!(startup.result.is_ok());
        drop(startup);
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        let before = fingerprint(&editor, &state);
        let prepared = editor
            .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
            .unwrap();
        let other = session.reserve_retirement().unwrap();
        assert!(matches!(session.reserve_retirement(), Err(WrapError::Busy)));
        let targets = prepared.navigation.as_ref().unwrap().targets.cursors().as_ptr();
        let occupied = budget.reserve(MAX_SESSION_BYTES - budget.retained_bytes()).unwrap();
        let attempts = budget.allocation_attempts();
        let returned = match prepared.retire() {
            ControlFlow::Break(owner) => owner,
            ControlFlow::Continue(()) => panic!("exhausted transfer budget allocated a Box"),
        };
        assert_eq!(budget.allocation_attempts(), attempts);
        assert_eq!(returned.navigation.as_ref().unwrap().targets.cursors().as_ptr(), targets);
        assert!(returned.permit.as_ref().unwrap().is_current());
        assert_eq!(fingerprint(&editor, &state), before);
        drop(occupied);
        // Both slots are already reserved. Success must consume the embedded
        // permit, rather than trying to admit a third slot or storing an active
        // permit inside that permit's own slot.
        assert!(matches!(returned.retire(), ControlFlow::Continue(())));
        assert_eq!(fingerprint(&editor, &state), before);
        let failure = session.stop().settle(Duration::ZERO).unwrap_err();
        assert!(matches!(failure.error, WrapError::TimedOut));
        drop(other);
        settle(failure.ticket);
    }

    #[test]
    fn wrap_prepared_retirement_close_after_boxing_restores_exact_owner_and_permit() {
        let (editor, state, budget, source, mut engine) = setup();
        let session = WrapSession::with_budget(budget.clone(), || {}).unwrap();
        let nav = navigation(&mut engine, source.as_ref(), &state, &budget);
        let current = state.fence;
        let prepared = editor
            .prepare_visual_cursor_commit(&state, nav, current, &budget, session.reserve_retirement().unwrap())
            .unwrap();
        let targets = prepared.navigation.as_ref().unwrap().targets.cursors().as_ptr();
        let reveal = prepared.navigation.as_ref().unwrap().reveal.rows().as_ptr();
        let before = fingerprint(&editor, &state);
        let attempts = budget.allocation_attempts();
        let mut ticket = None;
        let returned = match prepared.retire_before_publish(|| {
            ticket = Some(session.stop());
        }) {
            ControlFlow::Break(owner) => owner,
            ControlFlow::Continue(()) => panic!("closed publication accepted a prepared owner"),
        };
        assert_eq!(
            budget.allocation_attempts(),
            attempts + 1,
            "exercise the post-Box publication refusal, not just the early closed check"
        );
        assert_eq!(returned.navigation.as_ref().unwrap().targets.cursors().as_ptr(), targets);
        assert_eq!(returned.navigation.as_ref().unwrap().reveal.rows().as_ptr(), reveal);
        assert!(returned.permit.is_some());
        assert!(!returned.permit.as_ref().unwrap().is_current());
        assert_eq!(fingerprint(&editor, &state), before);
        let attempts = budget.allocation_attempts();
        let returned = match returned.retire() {
            ControlFlow::Break(owner) => owner,
            ControlFlow::Continue(()) => panic!("same closed owner unexpectedly retired"),
        };
        assert_eq!(budget.allocation_attempts(), attempts);
        assert_eq!(returned.navigation.as_ref().unwrap().targets.cursors().as_ptr(), targets);
        assert_eq!(returned.navigation.as_ref().unwrap().reveal.rows().as_ptr(), reveal);
        let failure = ticket.unwrap().settle(Duration::ZERO).unwrap_err();
        assert!(
            matches!(failure.error, WrapError::TimedOut),
            "restored permit must retain its outstanding reservation"
        );
        // This test caller now performs explicit off-UI owner cleanup; refusal
        // itself did not destroy the navigation, permit, or editor state.
        drop(returned);
        settle(failure.ticket);
    }
}
