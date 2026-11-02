//! Receipt-preserving updates. The plan describes only native differences;
//! surviving entries always retain the first transaction's Before value.
use super::*;

pub(super) struct Update {
    pub plan: AssociationPlan,
    pub receipt: AssociationReceipt,
    pub removed: SharedChanges,
}

pub(super) fn prepare(
    adapter: &mut impl AssociationAdapter,
    request: &AssociationRequest,
    previous: Option<&AssociationReceipt>,
) -> Result<Update, AssociationError> {
    let mut plan = plan(adapter, request)?;
    let Some(previous) = previous else {
        return Ok(Update {
            receipt: AssociationReceipt { owner: plan.owner.clone(), remaining: plan.changes.clone() },
            removed: SharedChanges::default(),
            plan,
        });
    };
    if previous.owner != plan.owner {
        return Err(err("association owner changed"));
    }
    let additions =
        plan.changes.iter().filter(|c| !previous.pending().iter().any(|old| old.location == c.location)).count();
    let removals = previous.pending().iter().filter(|c| !wanted(&c.location, request)).count();
    if previous.pending().len().saturating_add(additions) > MAX_CHANGES
        || plan.changes.len().saturating_add(removals) > MAX_CHANGES
    {
        return Err(err("combined update exceeds 768 values before allocation"));
    }
    // Engine::execute holds the temporary-work reservation throughout planning.
    let (mut mutations, mutation_charge) = plan.changes.copied()?;
    let (mut retained, receipt_charge) = previous.remaining.copied()?;
    mutations
        .try_reserve_exact(MAX_CHANGES.saturating_sub(mutations.len()))
        .map_err(|_| err("update allocation failed"))?;
    retained
        .try_reserve_exact(MAX_CHANGES.saturating_sub(retained.len()))
        .map_err(|_| err("receipt allocation failed"))?;
    let removed_charge = Admission::acquire(WORKSPACE_BYTES)?;
    let mut removed = Vec::new();
    removed.try_reserve_exact(MAX_CHANGES).map_err(|_| err("update allocation failed"))?;
    // Check every old entry, including those absent from the new native plan.
    // Before is also allowed after an interrupted, automatically rolled-back Apply.
    for old in previous.pending() {
        let current = read_one(adapter, &old.location)?;
        if current != old.before && !same_owned_value(&current, &old.after) {
            return Err(err("association changed outside Kwil; update refused"));
        }
        if !wanted(&old.location, request) {
            removed.push(old.clone());
            retained.retain(|c| c.location != old.location);
            if current != old.before {
                mutations.push(Change { location: old.location.clone(), before: current, after: old.before.clone() });
            }
        }
    }
    if let AssociationPlatform::Linux { config_home, .. } = &request.platform {
        let mimeapps = Location::UserFile(config_home.join("mimeapps.list"));
        if let Some(old) = previous.pending().iter().find(|c| c.location == mimeapps) {
            let current = read_one(adapter, &mimeapps)?;
            let source = current.as_ref().map(StoredValue::as_text).transpose()?.unwrap_or("");
            let original = old.before.as_ref().map(StoredValue::as_text).transpose()?.unwrap_or("");
            let mut after = mutations
                .iter()
                .find(|c| c.location == mimeapps)
                .and_then(|c| c.after.as_ref())
                .map(StoredValue::as_text)
                .transpose()?
                .unwrap_or(source)
                .to_owned();
            // Restore only removed MIME keys; shared XDG files remain one owned value.
            let target: BTreeSet<_> = request.types.iter().filter_map(|t| t.content_type.as_deref()).collect();
            let old_mimes = previous
                .pending()
                .iter()
                .filter_map(|c| match &c.location {
                    Location::UserFile(p) if p.file_name().is_some_and(|n| n == "kwil.desktop") => c.after.as_ref(),
                    _ => None,
                })
                .filter_map(|v| v.as_text().ok())
                .flat_map(|s| s.lines().filter_map(|l| l.strip_prefix("MimeType=")).flat_map(|l| l.split(';')));
            for mime in old_mimes.filter(|m| !m.is_empty() && !target.contains(m)) {
                for group in ["Added Associations", "Removed Associations", "Default Applications"] {
                    after = restore_ini_key(&after, original, group, mime)?;
                }
            }
            mutations.retain(|c| c.location != mimeapps);
            let value = ValueBudget::new(MAX_VALUE_BYTES).copy(0, after.as_bytes())?;
            if current.as_ref() != Some(&value) {
                mutations.push(Change { location: mimeapps, before: current, after: Some(value) });
            }
        }
    }
    for change in &mutations {
        if removed.iter().any(|c| c.location == change.location) {
            continue;
        }
        if let Some(old) = retained.iter_mut().find(|c| c.location == change.location) {
            old.after = change.after.clone();
        } else {
            retained.push(change.clone());
        }
    }
    if retained.len().checked_add(removed.len()).is_none_or(|n| n > MAX_CHANGES) || mutations.len() > MAX_CHANGES {
        return Err(err("combined registration exceeds 256 types / 768 values"));
    }
    plan.changes = SharedChanges::admitted(mutations, mutation_charge);
    let receipt =
        AssociationReceipt { owner: plan.owner.clone(), remaining: SharedChanges::admitted(retained, receipt_charge) };
    let removed = SharedChanges::admitted(removed, removed_charge);
    worker::validate_receipt(&receipt).map_err(|e| err(e.to_string()))?;
    Ok(Update { plan, receipt, removed })
}

fn wanted(location: &Location, request: &AssociationRequest) -> bool {
    match location {
        Location::WindowsValue { subkey, .. } => request.types.iter().any(|t| {
            let ext = t.extension.trim_start_matches('.');
            subkey
                .strip_prefix("Kwil.")
                .or_else(|| subkey.strip_prefix('.'))
                .is_some_and(|tail| tail.split('\\').next().is_some_and(|kind| kind.eq_ignore_ascii_case(ext)))
        }),
        Location::MacRole { uti, .. } => request.types.iter().any(|t| t.content_type.as_ref() == Some(uti)),
        Location::UserFile(_) => true,
    }
}

fn restore_ini_key(current: &str, original: &str, group: &str, key: &str) -> Result<String, AssociationError> {
    let header = format!("[{group}]");
    let mut inside = false;
    let original_line = original.split_inclusive('\n').find(|line| {
        if line.trim().starts_with('[') {
            inside = line.trim() == header;
        }
        inside && line.split_once('=').is_some_and(|(k, _)| k.trim() == key)
    });
    let mut output = String::new();
    output
        .try_reserve_exact(current.len().saturating_add(original_line.map_or(0, str::len)))
        .map_err(|_| err("INI update allocation failed"))?;
    inside = false;
    for line in current.split_inclusive('\n') {
        if line.trim().starts_with('[') {
            inside = line.trim() == header;
        }
        if inside && line.split_once('=').is_some_and(|(k, _)| k.trim() == key) {
            if let Some(original) = original_line {
                output.push_str(original);
                if !original.ends_with('\n') && line.ends_with('\n') {
                    output.push_str(if line.ends_with("\r\n") { "\r\n" } else { "\n" });
                }
            }
        } else {
            output.push_str(line);
        }
    }
    if output.len() > MAX_VALUE_BYTES {
        return Err(err("updated MIME file exceeds limit"));
    }
    Ok(output)
}

pub(super) fn recovery_receipt(
    receipt: &AssociationReceipt,
    removed: &SharedChanges,
) -> Result<AssociationReceipt, AssociationError> {
    if receipt.remaining.len().checked_add(removed.len()).is_none_or(|n| n > MAX_CHANGES) {
        return Err(err("update recovery exceeds receipt limit before allocation"));
    }
    let (mut entries, charge) = receipt.remaining.copied()?;
    entries.try_reserve_exact(removed.len()).map_err(|_| err("recovery allocation failed"))?;
    entries.extend(removed.iter().cloned());
    if entries.len() > MAX_CHANGES {
        return Err(err("update recovery exceeds receipt limit"));
    }
    Ok(AssociationReceipt { owner: receipt.owner.clone(), remaining: SharedChanges::admitted(entries, charge) })
}

pub(super) fn undo_receipt(
    adapter: &mut impl AssociationAdapter,
    receipt: &AssociationReceipt,
    removed: &SharedChanges,
    mutations: &SharedChanges,
) -> Result<AssociationReceipt, AssociationError> {
    let complete = recovery_receipt(receipt, removed)?;
    let (mut entries, charge) = complete.remaining.copied()?;
    for entry in &mut entries {
        let current = read_one(adapter, &entry.location)?;
        if current != entry.before
            && !same_owned_value(&current, &entry.after)
            && mutations.iter().any(|m| m.location == entry.location && same_owned_value(&current, &m.before))
        {
            // A crash before this value's update left the preceding owned After.
            // Original Before stays unchanged; only this temporary Undo signature varies.
            entry.after = current;
        }
    }
    Ok(AssociationReceipt { owner: receipt.owner.clone(), remaining: SharedChanges::admitted(entries, charge) })
}

pub(super) fn validate_successor(
    old: &AssociationReceipt,
    next: &AssociationReceipt,
    removed: &SharedChanges,
    mutations: &SharedChanges,
) -> Result<(), AssociationError> {
    if old.owner != next.owner {
        return Err(err("update changes receipt owner"));
    }
    let combined = recovery_receipt(next, removed)?;
    for original in old.pending() {
        if let Some(retained) = combined.pending().iter().find(|c| c.location == original.location) {
            if retained.before != original.before {
                return Err(err("update discards original Before bytes"));
            }
        } else if original.before != original.after {
            return Err(err("update loses an owned rollback entry"));
        }
    }
    for mutation in mutations.iter() {
        let target = combined
            .pending()
            .iter()
            .find(|c| c.location == mutation.location)
            .ok_or_else(|| err("update mutation has no rollback owner"))?;
        let expected =
            if removed.iter().any(|c| c.location == mutation.location) { &target.before } else { &target.after };
        if &mutation.after != expected {
            return Err(err("mutation does not match target receipt"));
        }
        if let Some(previous) = old.pending().iter().find(|c| c.location == mutation.location) {
            if mutation.before != previous.before && !same_owned_value(&mutation.before, &previous.after) {
                return Err(err("mutation does not match preceding owned value"));
            }
        } else if mutation.before != target.before {
            return Err(err("new mutation loses its Before value"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kw46_update_mime_key_restoration_keeps_newline_boundaries() {
        let original = "[Added Associations]\ntext/plain=other.desktop;";
        let current = "[Added Associations]\ntext/plain=other.desktop;kwil.desktop;\napplication/x-log=kwil.desktop;\n";
        let restored = restore_ini_key(current, original, "Added Associations", "text/plain").unwrap();
        assert!(restored.contains("text/plain=other.desktop;\napplication/x-log=kwil.desktop;\n"));
    }
}
