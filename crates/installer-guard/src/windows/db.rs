use super::{check, wide};
use crate::{
    Deadline, Refusal, Result,
    identity::hex,
    model::Import,
    pe, require, stream,
    tables::{self, Database, Row},
};
use sha2::{Digest, Sha256};
use std::ptr::{null, null_mut};
use windows_sys::Win32::{Foundation::*, System::ApplicationInstallationAndServicing::*};

pub struct Msi(pub u32);
impl Msi {
    pub fn new(value: u32) -> Result<Self> {
        require(value != 0, "msi-api", "missing MSI handle")?;
        Ok(Self(value))
    }
}
impl Drop for Msi {
    fn drop(&mut self) {
        unsafe {
            MsiCloseHandle(self.0);
        }
    }
}

/// Exactly two calls: size query and one bounded fill. A changing required
/// length refuses; no retry loop can turn a moving target into evidence.
pub fn string(deadline: &Deadline, mut api: impl FnMut(*mut u16, *mut u32) -> u32) -> Result<String> {
    string_limit(deadline, 32767, &mut api)
}
fn string_limit(deadline: &Deadline, limit: u32, mut api: impl FnMut(*mut u16, *mut u32) -> u32) -> Result<String> {
    deadline.check()?;
    let mut empty = 0u16;
    let mut needed = 0;
    let status = api(&mut empty, &mut needed);
    require(status == ERROR_SUCCESS || status == ERROR_MORE_DATA, "msi-api", format!("string size status {status}"))?;
    require(needed < limit && limit <= 32767, "limit", "MSI string including NUL")?;
    let capacity = needed.checked_add(1).ok_or_else(|| Refusal::new("limit", "MSI string"))?;
    let mut bytes = vec![0u16; capacity as usize];
    let mut count = capacity;
    deadline.check()?;
    check(api(bytes.as_mut_ptr(), &mut count), "MSI string fill")?;
    require(
        count == needed && bytes[count as usize] == 0 && !bytes[..count as usize].contains(&0),
        "unstable-evidence",
        "MSI string length/NUL",
    )?;
    String::from_utf16(&bytes[..count as usize]).map_err(|_| Refusal::new("invalid-database", "invalid UTF-16"))
}
pub fn record_string(deadline: &Deadline, record: u32, field: u32) -> Result<String> {
    string(deadline, |buffer, count| unsafe { MsiRecordGetStringW(record, field, buffer, count) })
}
pub fn property(deadline: &Deadline, session: u32, name: &str) -> Result<String> {
    let name = wide(name)?;
    string(deadline, |buffer, count| unsafe { MsiGetPropertyW(session, name.as_ptr(), buffer, count) })
}

pub struct Db<'a> {
    pub handle: Msi,
    pub deadline: &'a Deadline,
}
impl<'a> Db<'a> {
    pub fn active(session: u32, deadline: &'a Deadline) -> Result<Self> {
        deadline.check()?;
        Ok(Self { handle: Msi::new(unsafe { MsiGetActiveDatabase(session) })?, deadline })
    }
    pub fn cached(path: &str, deadline: &'a Deadline) -> Result<Self> {
        let path = wide(path)?;
        let mut handle = 0;
        deadline.check()?;
        check(
            unsafe { MsiOpenDatabaseW(path.as_ptr(), MSIDBOPEN_READONLY, &mut handle) },
            "open registered cached MSI read-only",
        )?;
        Ok(Self { handle: Msi::new(handle)?, deadline })
    }
    fn view(&self, query: &str, params: u32) -> Result<Msi> {
        let query = wide(query)?;
        let mut view = 0;
        self.deadline.check()?;
        check(unsafe { MsiDatabaseOpenViewW(self.handle.0, query.as_ptr(), &mut view) }, "open database view")?;
        let view = Msi::new(view)?;
        self.deadline.check()?;
        check(unsafe { MsiViewExecute(view.0, params) }, "execute read-only SELECT")?;
        Ok(view)
    }
    fn fetch(&self, view: u32) -> Result<Option<Msi>> {
        self.deadline.check()?;
        let mut record = 0;
        match unsafe { MsiViewFetch(view, &mut record) } {
            ERROR_SUCCESS => Ok(Some(Msi::new(record)?)),
            ERROR_NO_MORE_ITEMS => Ok(None),
            status => Err(Refusal::new("msi-api", format!("fetch status {status}"))),
        }
    }
    fn info(&self, view: u32, kind: i32) -> Result<Vec<String>> {
        let mut record = 0;
        self.deadline.check()?;
        check(unsafe { MsiViewGetColumnInfo(view, kind, &mut record) }, "column info")?;
        let record = Msi::new(record)?;
        let count = unsafe { MsiRecordGetFieldCount(record.0) };
        require(count > 0 && count <= 17, "invalid-database", "column count")?;
        (1..=count).map(|i| record_string(self.deadline, record.0, i)).collect()
    }
    fn blob(&self, table: &str, key: &str, cap: u64) -> Result<Blob<'_, 'a>> {
        require(matches!(table, "Binary" | "Icon"), "internal-error", "stream table")?;
        let params = Msi::new(unsafe { MsiCreateRecord(1) })?;
        let key_wide = wide(key)?;
        self.deadline.check()?;
        check(unsafe { MsiRecordSetStringW(params.0, 1, key_wide.as_ptr()) }, "bind stream key")?;
        let view = self.view(&format!("SELECT `Data` FROM `{table}` WHERE `Name` = ?"), params.0)?;
        let record = self.fetch(view.0)?.ok_or_else(|| Refusal::new("invalid-database", "missing stream"))?;
        require(unsafe { MsiRecordIsNull(record.0, 1) } == 0, "invalid-database", "NULL stream")?;
        let size = unsafe { MsiRecordDataSize(record.0, 1) } as u64;
        require(size > 0 && size <= cap, "limit", format!("{table}.{key} bytes"))?;
        require(self.fetch(view.0)?.is_none(), "invalid-database", "duplicate stream")?;
        Ok(Blob { db: self, table: table.to_owned(), key: key.to_owned(), _view: view, record, size })
    }
}

impl Database for Db<'_> {
    fn column_info(&self, table: &str) -> Result<Vec<(String, String)>> {
        require(tables::ALLOWED_TABLES.contains(&table), "internal-error", "column-info table")?;
        let view = self.view(&format!("SELECT * FROM `{table}`"), 0)?;
        let names = self.info(view.0, MSICOLINFO_NAMES)?;
        let types = self.info(view.0, MSICOLINFO_TYPES)?;
        require(names.len() == types.len(), "invalid-database", "column-info lengths")?;
        Ok(names.into_iter().zip(types).collect())
    }
    fn tables(&self) -> Result<Vec<String>> {
        let view = self.view("SELECT `Name` FROM `_Tables`", 0)?;
        let mut tables = Vec::with_capacity(tables::ALLOWED_TABLES.len());
        while let Some(record) = self.fetch(view.0)? {
            require(tables.len() < tables::ALLOWED_TABLES.len(), "limit", "authored tables")?;
            let name = record_string(self.deadline, record.0, 1)?;
            require(!tables.contains(&name), "invalid-database", "duplicate table")?;
            tables.push(name);
        }
        Ok(tables)
    }
    fn rows(&self, table: &str, columns: &[&str], cap: usize) -> Result<Vec<Row>> {
        require(tables::ALLOWED_TABLES.contains(&table), "internal-error", "table read allowlist")?;
        let stream = matches!(table, "Binary" | "Icon");
        let query = if stream { format!("SELECT `Name` FROM `{table}`") } else { format!("SELECT * FROM `{table}`") };
        let view = self.view(&query, 0)?;
        let names = self.info(view.0, MSICOLINFO_NAMES)?;
        require(
            names.iter().map(String::as_str).eq(columns.iter().copied()),
            "invalid-database",
            format!("{table} column names/order"),
        )?;
        let types = self.info(view.0, MSICOLINFO_TYPES)?;
        require(types.len() == columns.len(), "invalid-database", "column type count")?;
        let mut rows = Vec::with_capacity(cap);
        let mut inventory_bytes = 8usize;
        while let Some(record) = self.fetch(view.0)? {
            require(rows.len() < cap, "limit", format!("{table} row count"))?;
            require(
                unsafe { MsiRecordGetFieldCount(record.0) } as usize == columns.len(),
                "invalid-database",
                "record width",
            )?;
            let mut row = Vec::with_capacity(columns.len());
            for (i, ty) in types.iter().enumerate() {
                let field = i as u32 + 1;
                let first =
                    *ty.as_bytes().first().ok_or_else(|| Refusal::new("invalid-database", "empty column type"))?;
                require(
                    matches!(first, b'i' | b'I' | b's' | b'S' | b'l' | b'L'),
                    "invalid-database",
                    "unsupported column type",
                )?;
                if unsafe { MsiRecordIsNull(record.0, field) } != 0 {
                    require(first.is_ascii_uppercase(), "invalid-database", format!("{table} nonnullable cell"))?;
                    row.push(None);
                } else if matches!(first, b'i' | b'I') {
                    self.deadline.check()?;
                    let value = unsafe { MsiRecordGetInteger(record.0, field) };
                    require(value != i32::MIN, "invalid-database", "integer cell")?;
                    row.push(Some(value.to_string()));
                } else {
                    let width = ty
                        .get(1..)
                        .and_then(|s| s.parse::<usize>().ok())
                        .ok_or_else(|| Refusal::new("invalid-database", "string column width"))?;
                    require(width < 32767, "limit", "declared MSI column width")?;
                    let mut limit = if width == 0 { 32767 } else { width as u32 + 1 };
                    if table == "KwilResource" && columns[i] == "Data" {
                        limit = limit.min(4096);
                    }
                    if table == "_Validation" {
                        limit = limit.min(match columns[i] {
                            "Table" | "Column" | "Category" => 33,
                            "Nullable" => 5,
                            _ => 256,
                        });
                    }
                    let value = string_limit(self.deadline, limit, |buffer, count| unsafe {
                        MsiRecordGetStringW(record.0, field, buffer, count)
                    })?;
                    require(
                        width == 0 || value.encode_utf16().count() <= width,
                        "invalid-database",
                        "overlength column",
                    )?;
                    if table == "KwilResource" && columns[i] == "Data" {
                        require(value.encode_utf16().count() < 4096, "limit", "registry data cell")?;
                    }
                    row.push(Some(value));
                }
            }
            if table == "KwilResource" {
                for value in &row {
                    let value =
                        value.as_deref().ok_or_else(|| Refusal::new("invalid-inventory", "NULL resource cell"))?;
                    inventory_bytes = inventory_bytes
                        .checked_add(4)
                        .and_then(|n| n.checked_add(value.len()))
                        .ok_or_else(|| Refusal::new("limit", "inventory byte overflow"))?;
                    require(inventory_bytes <= 1_048_576, "limit", "canonical inventory bytes")?;
                }
            }
            rows.push(row);
        }
        Ok(rows)
    }
    fn custom_schema(&self, table: &str, columns: &[(&str, &str)], keys: &[&str]) -> Result<()> {
        require(tables::ALLOWED_TABLES.contains(&table), "internal-error", "schema table")?;
        let view = self.view(&format!("SELECT * FROM `{table}`"), 0)?;
        require(
            self.info(view.0, MSICOLINFO_NAMES)?.iter().map(String::as_str).eq(columns.iter().map(|(n, _)| *n))
                && self
                    .info(view.0, MSICOLINFO_TYPES)?
                    .iter()
                    .map(String::as_str)
                    .eq(columns.iter().map(|(_, ty)| *ty)),
            "invalid-database",
            format!("{table} exact typed schema"),
        )?;
        let table = wide(table)?;
        let mut record = 0;
        self.deadline.check()?;
        check(unsafe { MsiDatabaseGetPrimaryKeysW(self.handle.0, table.as_ptr(), &mut record) }, "primary keys")?;
        let record = Msi::new(record)?;
        require(
            unsafe { MsiRecordGetFieldCount(record.0) } as usize == keys.len(),
            "invalid-database",
            "primary key count",
        )?;
        for (i, key) in keys.iter().enumerate() {
            require(
                record_string(self.deadline, record.0, i as u32 + 1)? == *key,
                "invalid-database",
                "primary key order",
            )?;
        }
        Ok(())
    }
    fn summary(&self, property: u32) -> Result<String> {
        let mut summary = 0;
        self.deadline.check()?;
        check(unsafe { MsiGetSummaryInformationW(self.handle.0, null(), 0, &mut summary) }, "read-only summary")?;
        let summary = Msi::new(summary)?;
        let mut ty = 0;
        let mut value = 0;
        if matches!(property, 14 | 15) {
            let mut count = 0;
            self.deadline.check()?;
            check(
                unsafe {
                    MsiSummaryInfoGetPropertyW(
                        summary.0,
                        property,
                        &mut ty,
                        &mut value,
                        null_mut(),
                        null_mut(),
                        &mut count,
                    )
                },
                "integer summary",
            )?;
            require(ty == 2 || ty == 3, "invalid-database", "integer summary type")?;
            Ok(value.to_string())
        } else {
            let text = string(self.deadline, |buffer, count| unsafe {
                MsiSummaryInfoGetPropertyW(summary.0, property, &mut ty, &mut value, null_mut(), buffer, count)
            })?;
            require(ty == 30, "invalid-database", "string summary type")?;
            Ok(text)
        }
    }
    fn stream_hash(&self, table: &str, key: &str, cap: u64) -> Result<(u64, String)> {
        let blob = self.blob(table, key, cap)?;
        let mut count = 0u64;
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 65536];
        loop {
            self.deadline.check()?;
            let mut n = bytes.len() as u32;
            check(unsafe { MsiRecordReadStream(blob.record.0, 1, bytes.as_mut_ptr(), &mut n) }, "hash MSI stream")?;
            require(n as usize <= bytes.len(), "msi-api", "stream read length")?;
            if n == 0 {
                break;
            }
            count = count.checked_add(n as u64).ok_or_else(|| Refusal::new("limit", "stream bytes"))?;
            require(count <= blob.size && count <= cap, "limit", "stream grew")?;
            hash.update(&bytes[..n as usize]);
        }
        require(count == blob.size, "unstable-evidence", "MSI stream length")?;
        Ok((count, hex(&hash.finalize())))
    }
    fn guard_evidence(&self, architecture: &str, abi: u32) -> Result<Vec<Import>> {
        let mut stream = stream::Window::new(self.blob("Binary", "KwilPreflight", 16_777_216)?)?;
        match abi {
            1 => pe::inspect(&mut stream, architecture),
            2 => pe::inspect_v2(&mut stream, architecture),
            _ => Err(Refusal::new("schema-unsupported", "guard ABI")),
        }
    }
}

struct Blob<'db, 'clock> {
    db: &'db Db<'clock>,
    table: String,
    key: String,
    _view: Msi,
    record: Msi,
    size: u64,
}
impl stream::Sequential for Blob<'_, '_> {
    fn len(&self) -> u64 {
        self.size
    }
    fn check(&self) -> Result<()> {
        self.db.deadline.check()
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<usize> {
        self.db.deadline.check()?;
        require(!bytes.is_empty() && bytes.len() <= stream::CHUNK, "limit", "MSI stream read window")?;
        let mut count = bytes.len() as u32;
        check(
            unsafe { MsiRecordReadStream(self.record.0, 1, bytes.as_mut_ptr(), &mut count) },
            "read MSI stream window",
        )?;
        require(count as usize <= bytes.len(), "msi-api", "stream read length")?;
        Ok(count as usize)
    }

    fn rewind(&mut self) -> Result<()> {
        self.db.deadline.check()?;
        let fresh = self.db.blob(&self.table, &self.key, self.size)?;
        require(fresh.size == self.size, "unstable-evidence", "Binary stream resized")?;
        // Parameters remain pinned to this same database/table/key. Drop the
        // old record before its view, and never discover another cache path.
        self.record = fresh.record;
        self._view = fresh._view;
        Ok(())
    }
}
