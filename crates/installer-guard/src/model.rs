use crate::{Refusal, Result, identity::*, require};

pub const REQUIRED_FILES: [&str; 7] =
    ["kwil.exe", "kwil.com", "kwil.ico", "LICENSE", "LICENSES.md", "LICENSES.txt", "kwil.no-update"];
pub const NO_UPDATE: &[u8] = b"Updates are managed by this distribution. Rebuild/reinstall explicitly.\n";
pub const SHELL_KEY: &str = "Software\\Classes\\*\\shell\\Kwil.3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource(pub [String; 13]);
impl Resource {
    pub fn id(&self) -> &str {
        &self.0[0]
    }
    pub fn kind(&self) -> &str {
        &self.0[1]
    }
    pub fn root(&self) -> &str {
        &self.0[2]
    }
    pub fn relative(&self) -> &str {
        &self.0[3]
    }
    pub fn name(&self) -> &str {
        &self.0[4]
    }
    pub fn data(&self) -> &str {
        &self.0[5]
    }
    pub fn size(&self) -> &str {
        &self.0[6]
    }
    pub fn hash(&self) -> &str {
        &self.0[7]
    }
    pub fn component(&self) -> &str {
        &self.0[8]
    }
    pub fn file_key(&self) -> &str {
        &self.0[10]
    }
    pub fn registry_key(&self) -> &str {
        &self.0[11]
    }
    pub fn identity(&self) -> String {
        match self.kind() {
            "F" => format!("{}:{}", self.root(), self.relative().to_ascii_lowercase()),
            "R" => format!("H:{}:{}", self.relative().to_ascii_lowercase(), self.name().to_ascii_lowercase()),
            "K" => format!("H:{}:#key", self.relative().to_ascii_lowercase()),
            _ => String::new(),
        }
    }
    fn blank(kind: &str, root: &str, relative: &str, component: &str) -> Self {
        let mut row = Self(std::array::from_fn(|_| "-".to_owned()));
        for (i, s) in [(1, kind), (2, root), (3, relative), (8, component), (12, "0")] {
            row.0[i] = s.to_owned();
        }
        row
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Import {
    pub load: String,
    pub dll: String,
}
pub fn validate_imports(imports: &[Import]) -> Result<()> {
    require(!imports.is_empty() && imports.len() <= 128, "limit", "guard import count")?;
    for (i, import) in imports.iter().enumerate() {
        let n = &import.dll;
        require(
            matches!(import.load.as_str(), "N" | "D")
                && n.len() > 4
                && n.len() <= 128
                && n.ends_with(".dll")
                && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)),
            "invalid-metadata",
            "guard import",
        )?;
        require(i == 0 || imports[i - 1] < *import, "invalid-metadata", "duplicate or unsorted imports")?;
        require(
            !["vcruntime", "msvcp", "msvcr", "ucrtbase", "api-ms-win-crt-"].iter().any(|prefix| n.starts_with(prefix)),
            "dynamic-runtime",
            n,
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Package {
    pub architecture: String,
    pub version: String,
    pub commit: String,
    pub target: String,
    pub guard_hash: String,
    pub inventory_hash: String,
    pub build_id: String,
    pub product_code: String,
    pub package_code: String,
    pub imports: Vec<Import>,
    pub resources: Vec<Resource>,
}

pub fn shortcut() -> [u8; 868] {
    let mut out = [0u8; 868];
    out[..4].copy_from_slice(&76u32.to_le_bytes());
    out[4..20].copy_from_slice(&[1, 20, 2, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
    out[20..24].copy_from_slice(&0x02000200u32.to_le_bytes());
    out[60..64].copy_from_slice(&1u32.to_le_bytes());
    out[76..80].copy_from_slice(&788u32.to_le_bytes());
    out[80..84].copy_from_slice(&0xa0000001u32.to_le_bytes());
    let target = b"%LOCALAPPDATA%\\Programs\\Kwil\\kwil.exe";
    out[84..84 + target.len()].copy_from_slice(target);
    for (i, b) in target.iter().enumerate() {
        out[344 + i * 2] = *b;
    }
    out
}

pub fn inventory_bytes(rows: &[Resource]) -> Result<Vec<u8>> {
    require(rows.len() <= 512, "limit", "resource count")?;
    let mut out = b"KWR1".to_vec();
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        for field in &row.0 {
            lp(&mut out, field)?;
        }
    }
    Ok(out)
}

impl Package {
    pub fn version_numbers(&self) -> Result<[u32; 3]> {
        version(&self.version)
    }
    pub fn build_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = b"KWB1".to_vec();
        for s in
            ["1", "1", FAMILY, &self.architecture, &self.version, &self.commit, &self.target, "1", &self.guard_hash]
        {
            lp(&mut bytes, s)?;
        }
        bytes.extend_from_slice(&(self.imports.len() as u32).to_le_bytes());
        for import in &self.imports {
            lp(&mut bytes, &import.load)?;
            lp(&mut bytes, &import.dll)?;
        }
        let inventory = inventory_bytes(&self.resources)?;
        require(
            bytes
                .len()
                .checked_add(4)
                .and_then(|n| n.checked_add(inventory.len()))
                .is_some_and(|n| n <= INVENTORY_LIMIT + 65536),
            "limit",
            "build encoding",
        )?;
        bytes.extend_from_slice(&(inventory.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&inventory);
        Ok(bytes)
    }
    pub fn validate(&self) -> Result<()> {
        self.version_numbers()?;
        let target = match self.architecture.as_str() {
            "x64" => "x86_64-pc-windows-msvc",
            "arm64" => "aarch64-pc-windows-msvc",
            _ => return Err(Refusal::new("architecture-conflict", "unsupported package architecture")),
        };
        require(self.target == target && is_hex(&self.commit, 40), "invalid-metadata", "target/commit")?;
        for hash in [&self.guard_hash, &self.inventory_hash, &self.build_id] {
            require(is_hex(hash, 64), "invalid-metadata", "SHA-256")?;
        }
        package_guid(&self.package_code)?;
        validate_imports(&self.imports)?;
        require(self.resources.len() == 29, "invalid-inventory", "schema-1 resource count")?;
        let expected = reconstruct(&self.architecture, &self.resources)?;
        require(self.resources == expected, "invalid-inventory", "schema-1 resource model/identities/components")?;
        require(sha256(&inventory_bytes(&self.resources)?) == self.inventory_hash, "hash-mismatch", "inventory")?;
        require(sha256(&self.build_bytes()?) == self.build_id, "hash-mismatch", "BuildID")?;
        require(
            product(&self.architecture, &self.version, &self.build_id)? == self.product_code,
            "hash-mismatch",
            "ProductCode",
        )
    }
}

/// Reconstruct every schema-1 cell independently. Only the seven payload file
/// hashes/sizes come from the package; their names and all ownership data are fixed.
pub fn reconstruct(arch: &str, supplied: &[Resource]) -> Result<Vec<Resource>> {
    require(supplied.len() <= 512, "limit", "inventory rows")?;
    let mut rows = Vec::with_capacity(29);
    let mut total = 0u64;
    for (root, name) in REQUIRED_FILES.into_iter().map(|n| ("I", n)).chain(std::iter::once(("P", "Kwil.lnk"))) {
        let mut matches = supplied.iter().filter(|r| r.kind() == "F" && r.root() == root && r.relative() == name);
        let input = matches.next().ok_or_else(|| Refusal::new("invalid-inventory", name))?;
        require(matches.next().is_none(), "invalid-inventory", "duplicate file")?;
        let size = decimal(input.size(), 536_870_912)?;
        require(is_hex(input.hash(), 64), "invalid-inventory", name)?;
        total = total.checked_add(size).ok_or_else(|| Refusal::new("limit", "installed bytes"))?;
        require(total <= 1_073_741_824, "limit", "total installed bytes")?;
        if name == "kwil.no-update" {
            require(
                size == NO_UPDATE.len() as u64 && input.hash() == sha256(NO_UPDATE),
                "invalid-inventory",
                "no-update marker bytes",
            )?;
        }
        if root == "P" {
            require(size == 868 && input.hash() == sha256(&shortcut()), "invalid-inventory", "start-menu-v1 bytes")?;
        }
        let identity = format!("{root}:{}", name.to_ascii_lowercase());
        let (cid, guid) = component(arch, "F", &identity)?;
        let mut f = Resource::blank("F", root, name, &cid);
        f.0[6] = input.size().to_owned();
        f.0[7] = input.hash().to_owned();
        f.0[9] = if root == "I" { "p" } else { "g" }.to_owned();
        rows.push(f);
        let key = format!("Software\\Kwil\\Installer\\{FAMILY}\\Components\\{guid}");
        let mut r = Resource::blank("R", "H", &key, &cid);
        r.0[4] = "n:Owner".to_owned();
        r.0[5] = format!("{FAMILY}|{arch}|1|{identity}");
        r.0[12] = "1".to_owned();
        rows.push(r);
    }
    for (suffix, name, data) in [
        ("", "@", "Open with Kwil"),
        ("", "n:Icon", "\"${I}kwil.exe\",0"),
        ("\\command", "@", "\"${I}kwil.exe\" \"%1\""),
    ] {
        let key = format!("{SHELL_KEY}{suffix}");
        let identity = format!("H:{}:{}", key.to_ascii_lowercase(), name.to_ascii_lowercase());
        let (cid, _) = component(arch, "R", &identity)?;
        let mut r = Resource::blank("R", "H", &key, &cid);
        r.0[4] = name.to_owned();
        r.0[5] = data.to_owned();
        r.0[12] = "1".to_owned();
        rows.push(r);
    }
    let mut values: Vec<_> = rows.iter().filter(|r| r.kind() == "R").cloned().collect();
    values.sort_by_key(Resource::identity);
    for r in values {
        if !rows.iter().any(|k| k.kind() == "K" && k.relative() == r.relative()) {
            rows.push(Resource::blank("K", "H", r.relative(), r.component()));
        }
    }
    rows.sort_by_key(Resource::identity);
    for (i, row) in rows.iter_mut().enumerate() {
        let digits = format!("{:06}", i + 1);
        row.0[0] = format!("r{digits}");
        if row.kind() == "F" {
            row.0[10] = format!("F{digits}");
        }
        if row.kind() == "R" {
            row.0[11] = format!("R{digits}");
        }
    }
    Ok(rows)
}

/// Called only on the independently reconstructed, validated schema-1 model.
/// Owner's braced FAMILY is an exact literal exception, not a template token.
/// Equality with reconstruct() rejects all arbitrary dollar/braces in metadata.
pub fn registry_bytes(resource: &Resource, install: &str) -> Result<Vec<u8>> {
    require(resource.kind() == "R" && install.ends_with('\\'), "invalid-inventory", "registry expansion")?;
    let tokens = resource.data().matches("${I}").count();
    let units = if resource.data() == "~empty~" {
        Some(0)
    } else {
        resource.data().encode_utf16().count().checked_sub(tokens * 4).and_then(|n| {
            install.encode_utf16().count().checked_mul(tokens).and_then(|expanded| n.checked_add(expanded))
        })
    }
    .ok_or_else(|| Refusal::new("limit", "registry expansion overflow"))?;
    require(units < 4096, "limit", "expanded REG_SZ including NUL")?;
    let text = if resource.data() == "~empty~" { String::new() } else { resource.data().replace("${I}", install) };
    require(!text.contains('\0') && text.encode_utf16().count() < 4096, "limit", "registry data")?;
    Ok(text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect())
}

pub fn short_long(row: &Resource) -> String {
    let extension = row
        .relative()
        .rsplit_once('.')
        .map(|(_, e)| {
            e.bytes()
                .filter(|b| b.is_ascii_alphanumeric())
                .take(3)
                .map(|b| b.to_ascii_lowercase() as char)
                .collect::<String>()
        })
        .unwrap_or_default();
    let suffix = if extension.is_empty() { String::new() } else { format!(".{extension}") };
    format!("K{}{suffix}|{}", &row.id()[1..], row.relative())
}
