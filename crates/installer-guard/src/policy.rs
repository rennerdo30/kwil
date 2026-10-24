use crate::{
    Refusal, Result,
    model::{Package, Resource},
    require,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Install,
    Upgrade,
    Repair,
    Uninstall,
}
#[derive(Default, Clone, Debug)]
pub struct Session {
    pub remove: String,
    pub reinstall: String,
    pub reinstall_mode: String,
    pub installed: bool,
    pub action: String,
    pub main_action: i32,
    pub older: String,
    pub equal: String,
    pub newer: String,
}

pub fn operation(active: &Package, original: Option<&Package>, s: &Session) -> Result<Operation> {
    let maintenance = original.is_some_and(|p| p.product_code == active.product_code);
    require(s.action == "INSTALL" && s.installed == maintenance, "session-override", "ACTION/Installed")?;
    require(
        matches!(s.remove.as_str(), "" | "ALL") && matches!(s.reinstall.as_str(), "" | "ALL"),
        "unsupported-operation",
        "REMOVE/REINSTALL",
    )?;
    let op = if maintenance {
        let original = original.ok_or_else(|| Refusal::new("registration", "missing original"))?;
        require(
            active.package_code == original.package_code && active.build_id == original.build_id,
            "package-mismatch",
            &active.product_code,
        )?;
        match (s.remove.as_str(), s.reinstall.as_str(), s.reinstall_mode.as_str()) {
            ("ALL", "", "" | "omus") => Operation::Uninstall,
            ("", "ALL", "amus") => Operation::Repair,
            ("", "", "" | "omus") => return Err(Refusal::new("repair-required", &active.product_code)),
            _ => return Err(Refusal::new("unsupported-operation", "maintenance arguments")),
        }
    } else {
        require(
            s.remove.is_empty() && s.reinstall.is_empty() && matches!(s.reinstall_mode.as_str(), "" | "omus"),
            "unsupported-operation",
            "unregistered maintenance",
        )?;
        if let Some(original) = original {
            require(active.architecture == original.architecture, "architecture-conflict", &original.product_code)?;
            require(active.version_numbers()? > original.version_numbers()?, "version-conflict", &original.version)?;
            Operation::Upgrade
        } else {
            Operation::Install
        }
    };
    require(
        s.main_action == if op == Operation::Uninstall { 2 } else { 3 },
        "session-override",
        "Main feature action state",
    )?;
    Ok(op)
}

pub fn detection(op: Operation, original: Option<&Package>, s: &Session) -> Result<()> {
    let expected = if op == Operation::Upgrade { original.map(|p| p.product_code.as_str()).unwrap_or("") } else { "" };
    require(
        s.older == expected && s.equal.is_empty() && s.newer.is_empty(),
        "session-override",
        "KWIL_OLDER/EQUAL/NEWER",
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Absent,
    File { size: u64, sha256: String },
    Key,
    Value { value_type: u32, bytes: Vec<u8> },
}

pub trait Resources {
    /// Must inspect exact resources, their ancestors, redirections and stable
    /// identities; access denied/unsupported/racing observations are errors.
    fn observe(&mut self, resource: &Resource) -> Result<Observation>;
    fn portable_marker_absent(&mut self) -> Result<bool>;
    fn in_use(&mut self, original: &Package) -> Result<()>;
    fn install_root(&self) -> &str;
}

fn matches_original(row: &Resource, seen: &Observation, install: &str) -> Result<bool> {
    use Observation::*;
    Ok(match (row.kind(), seen) {
        (_, Absent) => true,
        ("F", File { size, sha256 }) => {
            *size == crate::identity::decimal(row.size(), 536_870_912)? && sha256 == row.hash()
        }
        ("K", Key) => true,
        ("R", Value { value_type, bytes }) => *value_type == 1 && *bytes == crate::model::registry_bytes(row, install)?,
        _ => false,
    })
}

pub fn resources(active: &Package, original: Option<&Package>, api: &mut impl Resources) -> Result<()> {
    require(api.portable_marker_absent()?, "portable-marker", "I:kwil.portable")?;
    // Check ALL original resources, including ones absent from a later inventory.
    // Missing resources are repairable; present changed resources never are.
    if let Some(original) = original {
        for row in &original.resources {
            let seen = api.observe(row)?;
            let code = match row.kind() {
                "F" => "changed-owned-file",
                "R" => "changed-owned-value",
                _ => "changed-owned-key",
            };
            require(matches_original(row, &seen, api.install_root())?, code, row.identity())?;
        }
    }
    for row in &active.resources {
        let old = original.and_then(|p| p.resources.iter().find(|r| r.identity() == row.identity()));
        if let Some(old) = old {
            require(old.component() == row.component(), "component-mismatch", row.identity())?;
            // Reobserve against ORIGINAL contents, never incoming replacement.
            let seen = api.observe(row)?;
            require(matches_original(old, &seen, api.install_root())?, "changed-owned-value", row.identity())?;
        } else {
            require(api.observe(row)? == Observation::Absent, "foreign-resource", row.identity())?;
        }
    }
    if let Some(original) = original {
        api.in_use(original)?;
    }
    Ok(())
}
