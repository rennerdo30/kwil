//! Shared language picker and worker-prepared settings for GUI and TUI.
//!
//! ROOT hook: on the existing SETTINGS worker, call
//! `AcceptedLanguageMappings::prepare(document.file_types())` before publishing
//! an accepted settings candidate. An error rejects that entire candidate;
//! retain the last accepted settings AND mappings and surface the diagnostic.
//! A missing settings file produces the normal empty/default table. Never
//! substitute empty mappings after a parse, read or mapping-validation error.
//! Deliver the prepared value with the accepted settings event, then call
//! `HighlightSession::apply_mappings` and `update` on the frontend. Preparing
//! mappings compiles globs for validation; it never belongs on input/render.
//!
//! ROOT exports this module. Open either frontend's picker through
//! `HighlightSession::language_picker`, enumerate `items(query)`, and commit via
//! `apply_picker`. The captured document and input epoch fence stale actions.
//! `Automatic` removes only that document's override; extension persistence
//! edits the settings document and returns through the acceptance path above.
//! Catalogue membership promises a language identity, not an external grammar
//! download. Provider/provenance/loading policy stays with the existing worker.
use crate::{
    Workspace,
    highlighting::{Detection, DocumentId, Language, LanguageChoice, LanguageMapping},
};
use kwil_core_commands::settings::SettingsError;
use kwil_core_syntax::detection::{Detector, MAX_MAPPINGS, MAX_PATTERN_BYTES};
use std::{collections::BTreeMap, sync::Arc};

/// Constructed on a settings worker, moved into a frontend without parsing or
/// matching. Private storage prevents bypassing all-or-nothing validation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcceptedLanguageMappings {
    mappings: Arc<[LanguageMapping]>,
}

impl AcceptedLanguageMappings {
    /// Stable BTreeMap order is the existing detector's first-match order.
    /// Accepts its bare extension and glob forms; the settings editor owns
    /// which forms it can author. Aliases resolve to the catalogue identity.
    pub fn prepare(file_types: &BTreeMap<String, String>) -> Result<Self, SettingsError> {
        if file_types.len() > MAX_MAPPINGS {
            return Err(SettingsError(format!("file_types: at most {MAX_MAPPINGS} mappings are supported")));
        }
        let mut mappings = Vec::with_capacity(file_types.len());
        for (pattern, name) in file_types {
            if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
                return Err(SettingsError(format!("file_types: pattern must contain 1 to {MAX_PATTERN_BYTES} bytes")));
            }
            // SettingsDocument bounds identifiers too. Keep this API bounded
            // for callers that prepare a candidate table directly.
            if name.len() > 128 {
                return Err(SettingsError(format!("file_types.{pattern}: language identifier exceeds 128 bytes")));
            }
            let language = Language::from_id(name)
                .ok_or_else(|| SettingsError(format!("file_types.{pattern}: unsupported language {name:?}")))?;
            mappings.push(LanguageMapping { pattern: pattern.clone(), language });
        }
        Detector::new(&mappings).map_err(|error| SettingsError(format!("file_types: {error}")))?;
        Ok(Self { mappings: mappings.into() })
    }

    pub fn mappings(&self) -> &[LanguageMapping] {
        &self.mappings
    }

    pub(crate) fn shared(&self) -> Arc<[LanguageMapping]> {
        self.mappings.clone()
    }
}

/// Detection is available only for the current document/path/settings/choice.
/// Until the worker replies, an explicit choice can still label the status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageStatus {
    pub choice: LanguageChoice,
    pub detection: Option<Detection>,
}

impl LanguageStatus {
    pub fn label(self) -> &'static str {
        self.choice.language().or(self.detection.map(|detection| detection.language)).map_or("Automatic", Language::id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguagePickerItem {
    pub choice: LanguageChoice,
    pub label: &'static str,
    pub selected: bool,
    /// The automatic detector's current result, independent of selection.
    pub detected: bool,
}

/// Small immutable ticket. No snapshots, source reads, grammar loading or
/// document-sized state. A tab switch/close/reopen invalidates its action.
#[derive(Clone, Debug)]
pub struct LanguagePicker {
    document: DocumentId,
    epoch: u64,
    status: LanguageStatus,
}

impl LanguagePicker {
    pub(crate) fn capture(view: &Workspace, status: LanguageStatus) -> Self {
        Self { document: view.active_id(), epoch: view.input_epoch(), status }
    }

    pub fn document(&self) -> DocumentId {
        self.document
    }

    pub fn status(&self) -> LanguageStatus {
        self.status
    }

    pub fn is_current(&self, view: &Workspace) -> bool {
        self.document == view.active_id() && self.epoch == view.input_epoch()
    }

    /// One automatic row followed by the existing catalogue. Filtering matches
    /// stable ids, extensions and conventional filenames with no heap copies.
    pub fn items<'a>(&'a self, query: &'a str) -> impl Iterator<Item = LanguagePickerItem> + 'a {
        let automatic = std::iter::once(LanguageChoice::Automatic);
        automatic.chain(Language::CATALOGUE.iter().copied().map(LanguageChoice::Explicit)).filter_map(move |choice| {
            let label = choice.language().map_or("Automatic", Language::id);
            let query = query.trim();
            let matches = contains_ascii_case_insensitive(label, query)
                || choice.language().is_some_and(|language| {
                    language
                        .extensions()
                        .split_whitespace()
                        .chain(language.filenames().split_whitespace())
                        .any(|alias| contains_ascii_case_insensitive(alias, query))
                });
            matches.then_some(LanguagePickerItem {
                choice,
                label,
                selected: choice == self.status.choice,
                detected: self.status.choice == LanguageChoice::Automatic
                    && choice.language().is_some_and(|language| {
                        self.status.detection.is_some_and(|detection| detection.language == language)
                    }),
            })
        })
    }
}

fn contains_ascii_case_insensitive(value: &str, query: &str) -> bool {
    query.is_empty() || value.as_bytes().windows(query.len()).any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::View;
    use crate::highlighting::DetectionOrigin;

    #[test]
    fn mappings_resolve_aliases_and_preserve_detector_precedence() {
        let table = BTreeMap::from([
            ("*.special".into(), "py".into()),
            ("rs".into(), "generic".into()),
            ("trace".into(), "log".into()),
        ]);
        let prepared = AcceptedLanguageMappings::prepare(&table).unwrap();
        let detector = Detector::new(prepared.mappings()).unwrap();
        let source = b"#!/bin/sh\n".to_vec();
        for (path, language) in
            [("code.special", Language::Python), ("code.rs", Language::Generic), ("a.trace", Language::Log)]
        {
            let detection = detector.detect(std::path::Path::new(path), None, &source, &|| false).unwrap();
            assert_eq!(detection.language, language);
            assert_eq!(detection.origin, DetectionOrigin::UserMapping);
        }
    }

    #[test]
    fn invalid_mapping_candidate_never_becomes_an_accepted_partial_table() {
        let mut table = BTreeMap::from([("rs".into(), "python".into())]);
        let accepted = AcceptedLanguageMappings::prepare(&table).unwrap();
        table.insert("z".into(), "unavailable-grammar".into());
        assert!(AcceptedLanguageMappings::prepare(&table).unwrap_err().0.contains("unsupported language"));
        assert_eq!(accepted.mappings()[0].language, Language::Python);
        assert_eq!(accepted.mappings().len(), 1);
        assert!(AcceptedLanguageMappings::prepare(&BTreeMap::from([("[".into(), "rust".into())])).is_err());
        assert!(
            AcceptedLanguageMappings::prepare(&BTreeMap::from([("x".repeat(MAX_PATTERN_BYTES + 1), "rust".into())]))
                .is_err()
        );
        let excessive = (0..=MAX_MAPPINGS).map(|i| (format!("x{i}"), "rust".into())).collect();
        assert!(AcceptedLanguageMappings::prepare(&excessive).is_err());
        assert!(AcceptedLanguageMappings::prepare(&BTreeMap::new()).unwrap().mappings().is_empty());
    }

    #[test]
    fn picker_uses_the_catalogue_and_distinguishes_generic_from_automatic() {
        let view = Workspace::new(View::new(None)).unwrap();
        let picker = LanguagePicker::capture(
            &view,
            LanguageStatus { choice: LanguageChoice::Explicit(Language::Generic), detection: None },
        );
        let rows: Vec<_> = picker.items("").collect();
        assert_eq!(rows.len(), Language::CATALOGUE.len() + 1);
        assert_eq!(rows.iter().filter(|row| row.selected).count(), 1);
        assert!(!rows[0].selected);
        assert!(rows.iter().any(|row| row.selected && row.choice == LanguageChoice::Explicit(Language::Generic)));
        assert_eq!(picker.items("  PY  ").next().unwrap().choice, LanguageChoice::Explicit(Language::Python));
        assert!(picker.items("no such language").next().is_none());
    }
}
