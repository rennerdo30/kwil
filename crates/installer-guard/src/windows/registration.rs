use super::{paths, wide};
use crate::{
    Deadline, Refusal, Result,
    identity::{FAMILY, guid_bytes},
    require,
};
use std::ptr::null;
use windows_sys::Win32::{Foundation::*, System::ApplicationInstallationAndServicing::*};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub product: String,
    pub package: String,
    pub version: String,
    pub location: String,
    pub cache: String,
}

fn product_info(deadline: &Deadline, code: &str, sid: &str, context: i32, name: &str) -> Result<Option<String>> {
    let code = wide(code)?;
    let sid = wide(sid)?;
    let name = wide(name)?;
    let sid_ptr = if context == MSIINSTALLCONTEXT_MACHINE { null() } else { sid.as_ptr() };
    let mut empty = 0u16;
    let mut needed = 0;
    deadline.check()?;
    let status =
        unsafe { MsiGetProductInfoExW(code.as_ptr(), sid_ptr, context, name.as_ptr(), &mut empty, &mut needed) };
    if status == ERROR_UNKNOWN_PRODUCT {
        return Ok(None);
    }
    require(
        matches!(status, ERROR_SUCCESS | ERROR_MORE_DATA),
        "registration",
        format!("product info size status {status}"),
    )?;
    require(needed < 32767, "limit", "registration string including NUL")?;
    let mut buffer = vec![0u16; needed as usize + 1];
    let mut count = buffer.len() as u32;
    deadline.check()?;
    super::check(
        unsafe {
            MsiGetProductInfoExW(code.as_ptr(), sid_ptr, context, name.as_ptr(), buffer.as_mut_ptr(), &mut count)
        },
        "product info bounded fill",
    )?;
    require(
        count == needed && buffer[count as usize] == 0 && !buffer[..count as usize].contains(&0),
        "unstable-evidence",
        "product info string changed",
    )?;
    let value = String::from_utf16(&buffer[..count as usize])
        .map_err(|_| Refusal::new("registration", "invalid product info UTF-16"))?;
    Ok(Some(value))
}
fn mandatory(deadline: &Deadline, code: &str, sid: &str, name: &str) -> Result<String> {
    let value = product_info(deadline, code, sid, MSIINSTALLCONTEXT_USERUNMANAGED, name)?
        .ok_or_else(|| Refusal::new("registration", "registered product disappeared"))?;
    require(!value.is_empty(), "registration", name)?;
    Ok(value)
}

pub fn read(code: &str, sid: &str, deadline: &Deadline) -> Result<Option<Registration>> {
    guid_bytes(code)?;
    // Explicitly reject managed/machine registration for a known candidate.
    for context in [MSIINSTALLCONTEXT_USERMANAGED, MSIINSTALLCONTEXT_MACHINE] {
        require(product_info(deadline, code, sid, context, "LocalPackage")?.is_none(), "registration-context", code)?;
    }
    let cache = product_info(deadline, code, sid, MSIINSTALLCONTEXT_USERUNMANAGED, "LocalPackage")?;
    let code_wide = wide(code)?;
    deadline.check()?;
    let state = unsafe { MsiQueryProductStateW(code_wide.as_ptr()) };
    let Some(cache) = cache else {
        // ABSENT means installed for a different user, not permission to claim
        // its destination. ADVERTISED and broken/unknown statuses also refuse.
        require(state == INSTALLSTATE_UNKNOWN, "registration-context", format!("{code} state {state}"))?;
        return Ok(None);
    };
    require(state == INSTALLSTATE_DEFAULT, "registration-context", format!("{code} state {state}"))?;
    let sid_wide = wide(sid)?;
    let mut returned_code = [0u16; 39];
    let mut returned_sid = [0u16; 184];
    let mut sid_size = returned_sid.len() as u32;
    let mut context = 0;
    deadline.check()?;
    super::check(
        unsafe {
            MsiEnumProductsExW(
                code_wide.as_ptr(),
                sid_wide.as_ptr(),
                MSIINSTALLCONTEXT_USERUNMANAGED as u32,
                0,
                returned_code.as_mut_ptr(),
                &mut context,
                returned_sid.as_mut_ptr(),
                &mut sid_size,
            )
        },
        "verify registration SID/context",
    )?;
    require(
        sid_size < returned_sid.len() as u32 && context == MSIINSTALLCONTEXT_USERUNMANAGED && returned_code[38] == 0,
        "registration-context",
        "registration enumeration",
    )?;
    let actual_sid = String::from_utf16(&returned_sid[..sid_size as usize])
        .map_err(|_| Refusal::new("registration-context", "SID UTF-16"))?;
    let actual_code = String::from_utf16(&returned_code[..38])
        .map_err(|_| Refusal::new("registration-context", "ProductCode UTF-16"))?;
    require(actual_sid == sid && actual_code == code, "registration-context", "other-user/conflicting registration")?;
    sid_size = returned_sid.len() as u32;
    deadline.check()?;
    require(
        unsafe {
            MsiEnumProductsExW(
                code_wide.as_ptr(),
                sid_wide.as_ptr(),
                MSIINSTALLCONTEXT_USERUNMANAGED as u32,
                1,
                returned_code.as_mut_ptr(),
                &mut context,
                returned_sid.as_mut_ptr(),
                &mut sid_size,
            )
        } == ERROR_NO_MORE_ITEMS,
        "registration-context",
        "duplicate registration",
    )?;
    let package = mandatory(deadline, code, sid, "PackageCode")?;
    let version = mandatory(deadline, code, sid, "VersionString")?;
    let location = mandatory(deadline, code, sid, "InstallLocation")?;
    paths::local_path(&cache)?;
    require(
        !cache.is_empty() && cache.to_ascii_lowercase().ends_with(".msi"),
        "registration",
        "registered LocalPackage",
    )?;
    Ok(Some(Registration { product: code.to_owned(), package, version, location, cache }))
}

pub fn discover(active: &str, sid: &str, deadline: &Deadline) -> Result<Option<Registration>> {
    let family = wide(FAMILY)?;
    let mut candidates = Vec::with_capacity(17);
    for i in 0..=16u32 {
        deadline.check()?;
        let mut code = [0u16; 39];
        let status = unsafe { MsiEnumRelatedProductsW(family.as_ptr(), 0, i, code.as_mut_ptr()) };
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        super::check(status, "enumerate related products")?;
        require(i < 16 && code[38] == 0, "limit", "related product count/format")?;
        let text = String::from_utf16(&code[..38]).map_err(|_| Refusal::new("registration", "ProductCode UTF-16"))?;
        guid_bytes(&text)?;
        require(!candidates.contains(&text), "registration", "duplicate related product")?;
        candidates.push(text);
    }
    // The active product is always queried independently, including /x and a
    // related-products list empty during maintenance or nested uninstall.
    if !candidates.iter().any(|p| p == active) {
        candidates.push(active.to_owned());
    }
    let mut registration = None;
    for code in candidates {
        let found = read(&code, sid, deadline)?;
        if let Some(found) = found {
            require(registration.is_none(), "registration", "multiple installed family products")?;
            registration = Some(found);
        } else {
            require(code == active, "registration", "related product lacks current-user unmanaged registration")?;
        }
    }
    Ok(registration)
}

pub fn agrees(registration: &Registration, package: &crate::model::Package, install: &str) -> Result<()> {
    require(
        registration.product == package.product_code
            && registration.package == package.package_code
            && registration.version == package.version,
        "package-mismatch",
        "registration/cached original identity",
    )?;
    require(
        paths::same(&paths::directory(&registration.location)?, install)?,
        "registration",
        "canonical registered destination",
    )
}
