//! Typed database reconstruction shared by active and registered cached MSI.
use crate::{Refusal, Result, identity::*, model::*, require};
pub type Row = Vec<Option<String>>;

pub trait Database {
    fn tables(&self) -> Result<Vec<String>>;
    fn column_info(&self, table: &str) -> Result<Vec<(String, String)>>;
    /// SELECT *; verify exact column order, cap rows BEFORE pushing, reject
    /// overlength/non-UTF16/null-invalid cells. custom types/PK are checked below.
    fn rows(&self, table: &str, columns: &[&str], cap: usize) -> Result<Vec<Row>>;
    fn custom_schema(&self, table: &str, columns: &[(&str, &str)], keys: &[&str]) -> Result<()>;
    fn summary(&self, property: u32) -> Result<String>;
    fn stream_hash(&self, table: &str, key: &str, cap: u64) -> Result<(u64, String)>;
    fn guard_evidence(&self, architecture: &str) -> Result<Vec<Import>>;
}

pub const PACKAGE_COLUMNS: [(&str, &str); 15] = [
    ("Id", "i2"),
    ("Schema", "i2"),
    ("ComponentSchema", "i2"),
    ("Family", "s38"),
    ("Architecture", "s5"),
    ("Version", "s20"),
    ("Commit", "s40"),
    ("Target", "s32"),
    ("GuardAbi", "i2"),
    ("GuardSha256", "s64"),
    ("InventorySha256", "s64"),
    ("BuildId", "s64"),
    ("ProductCode", "s38"),
    ("PackageCode", "s38"),
    ("ResourceCount", "i4"),
];
pub const RESOURCE_COLUMNS: [(&str, &str); 13] = [
    ("Id", "s32"),
    ("Kind", "s1"),
    ("Root", "s1"),
    ("Relative", "s240"),
    ("Name", "s130"),
    ("Data", "s0"),
    ("Size", "s20"),
    ("Sha256", "s64"),
    ("Component_", "s72"),
    ("Source", "s1"),
    ("File_", "s72"),
    ("Registry_", "s72"),
    ("ValueType", "i2"),
];
pub const IMPORT_COLUMNS: [(&str, &str); 2] = [("Load", "s1"), ("Dll", "s128")];
pub const ALLOWED_TABLES: &[&str] = &[
    "_Validation",
    "Property",
    "LaunchCondition",
    "Directory",
    "Feature",
    "FeatureComponents",
    "Component",
    "File",
    "Registry",
    "RemoveFile",
    "Media",
    "Icon",
    "Binary",
    "CustomAction",
    "InstallExecuteSequence",
    "InstallUISequence",
    "Upgrade",
    "Error",
    "KwilPackage",
    "KwilResource",
    "KwilGuardImport",
];

fn cell(row: &Row, i: usize) -> Result<&str> {
    row.get(i).and_then(Option::as_deref).ok_or_else(|| Refusal::new("invalid-database", "NULL/absent required cell"))
}
fn strings(rows: Vec<Row>, width: usize) -> Result<Vec<Vec<String>>> {
    rows.into_iter()
        .map(|r| {
            require(r.len() == width, "invalid-database", "column count")?;
            (0..width).map(|i| cell(&r, i).map(str::to_owned)).collect()
        })
        .collect()
}
fn row(values: &[Option<&str>]) -> Row {
    values.iter().map(|s| s.map(str::to_owned)).collect()
}
fn exact(db: &impl Database, table: &str, columns: &[&str], mut expected: Vec<Row>) -> Result<()> {
    let mut actual = db.rows(table, columns, expected.len())?;
    actual.sort();
    expected.sort();
    require(actual == expected, "standard-table-mismatch", table)
}
pub fn read(db: &impl Database) -> Result<Package> {
    let tables = db.tables()?;
    require(
        tables.len() <= ALLOWED_TABLES.len() && tables.iter().all(|t| ALLOWED_TABLES.contains(&t.as_str())),
        "unsupported-table",
        "authored table allowlist",
    )?;
    for (name, columns, keys) in [
        ("KwilPackage", PACKAGE_COLUMNS.as_slice(), &["Id"][..]),
        ("KwilResource", RESOURCE_COLUMNS.as_slice(), &["Id"][..]),
        ("KwilGuardImport", IMPORT_COLUMNS.as_slice(), &["Load", "Dll"][..]),
    ] {
        db.custom_schema(name, columns, keys)?;
    }
    let package_names: Vec<_> = PACKAGE_COLUMNS.iter().map(|(n, _)| *n).collect();
    let packages = strings(db.rows("KwilPackage", &package_names, 1)?, 15)?;
    require(packages.len() == 1, "invalid-database", "one KwilPackage row")?;
    let p = &packages[0];
    require(
        p[0] == "1" && p[1] == "1" && p[2] == "1" && p[3] == FAMILY && p[8] == "1",
        "schema-unsupported",
        "family/schema/component-schema/guard-ABI",
    )?;
    require(decimal(&p[14], 512)? == 29, "invalid-inventory", "resource count")?;
    let resource_names: Vec<_> = RESOURCE_COLUMNS.iter().map(|(n, _)| *n).collect();
    let resources = strings(db.rows("KwilResource", &resource_names, 512)?, 13)?
        .into_iter()
        .map(|r| {
            let fields: [String; 13] = r.try_into().map_err(|_| Refusal::new("invalid-database", "resource width"))?;
            Ok(Resource(fields))
        })
        .collect::<Result<Vec<_>>>()?;
    // SQL row order has no identity. Canonical inventory order is reconstructed.
    let mut resources = resources;
    resources.sort_by_key(|r| (r.identity(), r.id().to_owned()));
    let imports = strings(db.rows("KwilGuardImport", &["Load", "Dll"], 128)?, 2)?
        .into_iter()
        .map(|r| Import { load: r[0].clone(), dll: r[1].clone() })
        .collect::<Vec<_>>();
    let mut imports = imports;
    imports.sort();
    let package = Package {
        architecture: p[4].clone(),
        version: p[5].clone(),
        commit: p[6].clone(),
        target: p[7].clone(),
        guard_hash: p[9].clone(),
        inventory_hash: p[10].clone(),
        build_id: p[11].clone(),
        product_code: p[12].clone(),
        package_code: p[13].clone(),
        imports,
        resources,
    };
    package.validate()?;
    let template = if package.architecture == "x64" { "x64;1033" } else { "Arm64;1033" };
    for (id, expected) in [(7, template), (9, package.package_code.as_str()), (14, "500"), (15, "10")] {
        require(db.summary(id)? == expected, "summary-mismatch", format!("SummaryInformation {id}"))?;
    }
    let (size, hash) = db.stream_hash("Binary", "KwilPreflight", 16_777_216)?;
    require(size > 0 && hash == package.guard_hash, "hash-mismatch", "Binary.KwilPreflight")?;
    require(
        db.guard_evidence(&package.architecture)? == package.imports,
        "import-mismatch",
        "Binary.KwilPreflight normal/delay imports",
    )?;
    standard(db, &package, &tables)?;
    Ok(package)
}

pub fn properties(p: &Package) -> Vec<(&'static str, String)> {
    [
        ("ProductName", "Kwil"),
        ("Manufacturer", "rennerdo30"),
        ("ProductLanguage", "1033"),
        ("UpgradeCode", FAMILY),
        ("ProductCode", &p.product_code),
        ("ProductVersion", &p.version),
        ("MSIINSTALLPERUSER", "1"),
        ("ARPNOMODIFY", "1"),
        ("ARPNOREPAIR", "1"),
        ("ARPPRODUCTICON", "KwilIcon"),
        ("ARPINSTALLLOCATION", "[INSTALLDIR]"),
        ("MSIRESTARTMANAGERCONTROL", "Disable"),
        ("REBOOT", "ReallySuppress"),
        ("KWIL_SCHEMA", "1"),
        ("KWIL_COMPONENT_SCHEMA", "1"),
        ("KWIL_GUARD_ABI", "1"),
        ("KWIL_FAMILY", FAMILY),
        ("KWIL_ARCH", &p.architecture),
        ("KWIL_BUILD_ID", &p.build_id),
        ("SecureCustomProperties", "KWIL_OLDER;KWIL_EQUAL;KWIL_NEWER"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}
pub const EMPTY_PROPERTIES: &[&str] = &[
    "ALLUSERS",
    "DISABLEROLLBACK",
    "RollbackDisabled",
    "SHORTFILENAMES",
    "TRANSFORMS",
    "PATCH",
    "ADDLOCAL",
    "ADDSOURCE",
    "ADVERTISE",
    "REMOVEFEATURES",
    "ADDDEFAULT",
    "COMPADDLOCAL",
    "COMPADDSOURCE",
    "FILEADDLOCAL",
    "FILEADDSOURCE",
];

pub fn effective_properties(p: &Package, mut get: impl FnMut(&str) -> Result<String>) -> Result<()> {
    for (name, value) in properties(p) {
        if name != "ARPINSTALLLOCATION" {
            require(get(name)? == value, "session-override", name)?;
        }
    }
    for name in EMPTY_PROPERTIES {
        require(get(name)?.is_empty(), "session-override", *name)?;
    }
    Ok(())
}

fn standard(db: &impl Database, p: &Package, tables: &[String]) -> Result<()> {
    exact(
        db,
        "Property",
        &["Property", "Value"],
        properties(p).iter().map(|(k, v)| row(&[Some(k), Some(v)])).collect(),
    )?;
    let launch = db.rows("LaunchCondition", &["Condition", "Description"], 1)?;
    require(
        launch.len() == 1
            && cell(&launch[0], 0)? == "NOT RollbackDisabled AND NOT DISABLEROLLBACK"
            && !cell(&launch[0], 1)?.is_empty(),
        "standard-table-mismatch",
        "rollback LaunchCondition",
    )?;
    exact(
        db,
        "Directory",
        &["Directory", "Directory_Parent", "DefaultDir"],
        vec![
            row(&[Some("TARGETDIR"), None, Some("SourceDir")]),
            row(&[Some("LocalAppDataFolder"), Some("TARGETDIR"), Some(".")]),
            row(&[Some("INSTALLPROGRAMS"), Some("LocalAppDataFolder"), Some("PROGRA~1|Programs")]),
            row(&[Some("INSTALLDIR"), Some("INSTALLPROGRAMS"), Some("Kwil")]),
            row(&[Some("ProgramMenuFolder"), Some("TARGETDIR"), Some(".")]),
        ],
    )?;
    exact(
        db,
        "Feature",
        &["Feature", "Feature_Parent", "Title", "Description", "Display", "Level", "Directory_", "Attributes"],
        vec![row(&[
            Some("Main"),
            None,
            Some("Kwil"),
            Some("Kwil"),
            Some("1"),
            Some("1"),
            Some("INSTALLDIR"),
            Some("0"),
        ])],
    )?;
    let files: Vec<_> = p.resources.iter().filter(|r| r.kind() == "F").collect();
    let values: Vec<_> = p.resources.iter().filter(|r| r.kind() == "R").collect();
    let mut file_rows = Vec::with_capacity(8);
    let mut aliases = Vec::with_capacity(16);
    for (index, f) in files.iter().enumerate() {
        let name = short_long(f);
        let (short, long) = name.split_once('|').ok_or_else(|| Refusal::new("invalid-inventory", "short name"))?;
        for spelling in [short, long] {
            let identity = format!("{}:{}", f.root(), spelling.to_ascii_lowercase());
            require(!aliases.contains(&identity), "invalid-inventory", "short/long alias collision")?;
            aliases.push(identity);
        }
        file_rows.push(row(&[
            Some(f.file_key()),
            Some(f.component()),
            Some(&name),
            Some(f.size()),
            None,
            None,
            Some("16384"),
            Some(&(index + 1).to_string()),
        ]));
    }
    exact(
        db,
        "File",
        &["File", "Component_", "FileName", "FileSize", "Version", "Language", "Attributes", "Sequence"],
        file_rows,
    )?;
    let registry_rows = values
        .iter()
        .map(|r| {
            row(&[
                Some(r.registry_key()),
                Some("1"),
                Some(r.relative()),
                if r.name() == "@" { None } else { Some(&r.name()[2..]) },
                Some(&r.data().replace("${I}", "[INSTALLDIR]")),
                Some(r.component()),
            ])
        })
        .collect();
    exact(db, "Registry", &["Registry", "Root", "Key", "Name", "Value", "Component_"], registry_rows)?;
    let mut components = Vec::with_capacity(11);
    for r in &values {
        let f = files.iter().find(|f| f.component() == r.component());
        let (cid, guid) = if let Some(f) = f {
            component(&p.architecture, "F", &f.identity())?
        } else {
            component(&p.architecture, "R", &r.identity())?
        };
        let directory = if f.is_some_and(|f| f.root() == "P") { "ProgramMenuFolder" } else { "INSTALLDIR" };
        require(cid == r.component(), "component-mismatch", r.identity())?;
        components.push(row(&[Some(&cid), Some(&guid), Some(directory), Some("260"), None, Some(r.registry_key())]));
    }
    exact(
        db,
        "Component",
        &["Component", "ComponentId", "Directory_", "Attributes", "Condition", "KeyPath"],
        components,
    )?;
    exact(
        db,
        "FeatureComponents",
        &["Feature_", "Component_"],
        values.iter().map(|r| row(&[Some("Main"), Some(r.component())])).collect(),
    )?;
    // Identifier is not an ownership input; validate the sole row's semantics.
    let removal = db.rows("RemoveFile", &["FileKey", "Component_", "FileName", "DirProperty", "InstallMode"], 1)?;
    require(removal.len() == 1, "standard-table-mismatch", "RemoveFile count")?;
    let r = &removal[0];
    let key = cell(r, 0)?;
    require(
        !key.is_empty()
            && key.len() <= 72
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && files.iter().any(|f| f.root() == "I" && Some(f.component()) == r[1].as_deref())
            && r[2].is_none()
            && r[3].as_deref() == Some("INSTALLDIR")
            && r[4].as_deref() == Some("2"),
        "standard-table-mismatch",
        "RemoveFile nonrecursive empty directory removal",
    )?;
    exact(
        db,
        "Media",
        &["DiskId", "LastSequence", "DiskPrompt", "Cabinet", "VolumeLabel", "Source"],
        vec![row(&[Some("1"), Some("8"), None, Some("#kwil.cab"), None, None])],
    )?;
    // Read only the name column for stream tables; rows() special-cases these
    // projections without materializing binary cells.
    for table in ["Binary", "Icon"] {
        db.custom_schema(table, &[("Name", "s72"), ("Data", "v0")], &["Name"])?;
    }
    exact(db, "Binary", &["Name"], vec![row(&[Some("KwilPreflight")])])?;
    exact(db, "Icon", &["Name"], vec![row(&[Some("KwilIcon")])])?;
    let icon =
        files.iter().find(|f| f.relative() == "kwil.ico").ok_or_else(|| Refusal::new("invalid-inventory", "icon"))?;
    let (icon_size, icon_hash) = db.stream_hash("Icon", "KwilIcon", 536_870_912)?;
    require(
        icon_size == decimal(icon.size(), 536_870_912)? && icon_hash == icon.hash(),
        "hash-mismatch",
        "Icon.KwilIcon",
    )?;
    exact(
        db,
        "CustomAction",
        &["Action", "Type", "Source", "Target"],
        vec![
            row(&[Some("KwilPreflight"), Some("1"), Some("KwilPreflight"), Some("KwilPreflightV1")]),
            row(&[Some("SetKwilArpLocation"), Some("51"), Some("ARPINSTALLLOCATION"), Some("[INSTALLDIR]")]),
        ],
    )?;
    exact(
        db,
        "Upgrade",
        &["UpgradeCode", "VersionMin", "VersionMax", "Language", "Attributes", "Remove", "ActionProperty"],
        vec![
            row(&[Some(FAMILY), None, Some(&p.version), None, Some("1"), Some("ALL"), Some("KWIL_OLDER")]),
            row(&[Some(FAMILY), Some(&p.version), Some(&p.version), None, Some("770"), None, Some("KWIL_EQUAL")]),
            row(&[Some(FAMILY), Some(&p.version), None, None, Some("2"), None, Some("KWIL_NEWER")]),
        ],
    )?;
    exact(db, "Error", &["Error", "Message"], vec![row(&[Some("25001"), Some("Kwil installer preflight: [2]. [3]")])])?;
    sequence(db, false)?;
    if tables.iter().any(|s| s == "InstallUISequence") {
        sequence(db, true)?;
    }
    // Inspect every metadata row and require one matching declaration for every
    // actual column, including _Validation itself. This is not an ICE run.
    require(tables.iter().any(|s| s == "_Validation"), "invalid-database", "missing _Validation")?;
    let validation = db.rows(
        "_Validation",
        &[
            "Table",
            "Column",
            "Nullable",
            "MinValue",
            "MaxValue",
            "KeyTable",
            "KeyColumn",
            "Category",
            "Set",
            "Description",
        ],
        ALLOWED_TABLES.len() * 15,
    )?;
    let mut actual = Vec::with_capacity(validation.len());
    for r in validation {
        actual.push((cell(&r, 0)?.to_owned(), cell(&r, 1)?.to_owned(), cell(&r, 2)?.to_owned()));
    }
    let mut expected = Vec::new();
    for table in tables {
        for (column, ty) in db.column_info(table)? {
            require(expected.len() < ALLOWED_TABLES.len() * 15, "limit", "validation column count")?;
            let first = ty.as_bytes().first().ok_or_else(|| Refusal::new("invalid-database", "empty column type"))?;
            let nullable = if first.is_ascii_uppercase() { "Y" } else { "N" };
            expected.push((table.to_owned(), column, nullable.to_owned()));
        }
    }
    actual.sort();
    expected.sort();
    require(actual == expected, "invalid-database", "_Validation column/nullability coverage")?;
    Ok(())
}

fn sequence(db: &impl Database, ui: bool) -> Result<()> {
    let table = if ui { "InstallUISequence" } else { "InstallExecuteSequence" };
    let allowed: &[(&str, i32)] = &[
        ("FindRelatedProducts", 25),
        ("LaunchConditions", 100),
        ("ValidateProductID", 700),
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("SetKwilArpLocation", 1010),
        ("MigrateFeatureStates", 1200),
        ("InstallValidate", 1400),
        ("KwilPreflight", 1490),
        ("InstallInitialize", 1500),
        ("RemoveExistingProducts", 1501),
        ("ProcessComponents", 1600),
        ("UnpublishFeatures", 1800),
        ("RemoveRegistryValues", 2600),
        ("RemoveFiles", 3500),
        ("RemoveFolders", 3600),
        ("InstallFiles", 4000),
        ("WriteRegistryValues", 5000),
        ("RegisterUser", 6000),
        ("RegisterProduct", 6100),
        ("PublishFeatures", 6300),
        ("PublishProduct", 6400),
        ("InstallFinalize", 6600),
        ("ExecuteAction", 1300),
    ];
    let rows = db.rows(table, &["Action", "Condition", "Sequence"], allowed.len())?;
    let mut seen = Vec::with_capacity(allowed.len());
    for r in &rows {
        let action = cell(r, 0)?;
        let Some((_, sequence)) = allowed.iter().find(|(name, _)| *name == action) else {
            return Err(Refusal::new("unsupported-action", action));
        };
        require(!seen.contains(&action) && cell(r, 2)? == sequence.to_string(), "sequence-mismatch", action)?;
        seen.push(action);
        require(r[1].is_none() || r[1].as_deref() == Some("1"), "sequence-mismatch", format!("{action} condition"))?;
        if ui {
            require(
                [
                    "FindRelatedProducts",
                    "LaunchConditions",
                    "ValidateProductID",
                    "CostInitialize",
                    "FileCost",
                    "CostFinalize",
                    "ExecuteAction",
                ]
                .contains(&action),
                "unsupported-action",
                action,
            )?;
        } else {
            require(action != "ExecuteAction", "unsupported-action", action)?;
        }
    }
    if !ui {
        for required in [
            "FindRelatedProducts",
            "LaunchConditions",
            "CostInitialize",
            "FileCost",
            "CostFinalize",
            "SetKwilArpLocation",
            "InstallValidate",
            "KwilPreflight",
            "InstallInitialize",
            "RemoveExistingProducts",
            "ProcessComponents",
            "UnpublishFeatures",
            "RemoveRegistryValues",
            "RemoveFiles",
            "InstallFiles",
            "WriteRegistryValues",
            "RegisterUser",
            "RegisterProduct",
            "PublishFeatures",
            "PublishProduct",
            "InstallFinalize",
        ] {
            require(seen.contains(&required), "sequence-mismatch", required)?;
        }
        for action in ["SetKwilArpLocation", "KwilPreflight"] {
            require(
                rows.iter().any(|r| r[0].as_deref() == Some(action) && r[1].as_deref() == Some("1")),
                "sequence-mismatch",
                action,
            )?;
        }
    }
    Ok(())
}
