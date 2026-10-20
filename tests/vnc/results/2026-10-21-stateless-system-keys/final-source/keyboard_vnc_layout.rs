//! Stateless shortcut labels from the installed Windows layout's WDK tables.
//!
//! `prepare` copies bounded data before capture is armed. `translate` never calls
//! ToUnicode, loads a DLL, or reads/mutates a window's composition state.
//! Structure contract: Microsoft's input/layout WDK samples and kbd.h.
//! Only the verified standard German and Chinese/US table backends are accepted.

use super::vnc_chord::Stroke;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

type Failure = &'static str;
const NONE: u16 = 0xf000;
const DEAD: u16 = 0xf001;
const LIGATURE: u16 = 0xf002;
const CAPLOK: u8 = 1;
const CAPLOKALTGR: u8 = 4;

#[derive(Clone, Debug)]
struct Row {
    attributes: u8,
    characters: Vec<u16>,
}

#[derive(Debug)]
struct Layout {
    modifier_columns: Vec<u8>,
    rows: Vec<Option<Row>>,
}

impl Layout {
    fn character(&self, vk: u16, modifiers: u8, caps: bool) -> Result<u32, Failure> {
        let row = self
            .rows
            .get(usize::from(vk))
            .and_then(Option::as_ref)
            .ok_or("layout-unmapped")?;
        if row.attributes & !(CAPLOK | CAPLOKALTGR) != 0 {
            return Err("layout-unsupported-attributes");
        }
        let mut bits = modifiers;
        // CAPLOK applies per key, even to German punctuation and digits. It is
        // ignored with AltGr unless the row explicitly specifies CAPLOKALTGR.
        if caps
            && ((row.attributes & CAPLOK != 0 && bits & !1 == 0)
                || (row.attributes & CAPLOKALTGR != 0 && bits & 6 == 6))
        {
            bits ^= 1;
        }
        let column = *self
            .modifier_columns
            .get(usize::from(bits))
            .ok_or("layout-unmapped")?;
        let value = *row
            .characters
            .get(usize::from(column))
            .ok_or("layout-unmapped")?;
        match value {
            NONE => Err("layout-unmapped"),
            DEAD => Err("layout-dead-key"),
            LIGATURE => Err("layout-ligature"),
            0..=0x1f | 0x7f..=0x9f => Err("control-character"),
            0xd800..=0xdfff => Err("layout-surrogate"),
            _ => Ok(u32::from(value)),
        }
    }

    fn translate(&self, stroke: &Stroke) -> Result<u32, Failure> {
        let caps = stroke.modifiers.caps.ok_or("caps-state-unavailable")?;
        let shift = u8::from(stroke.modifiers.shift);
        let altgr = stroke.modifiers.right_alt && stroke.modifiers.control && stroke.modifiers.alt;
        let value = if altgr {
            match self.character(stroke.vk, shift | 6, caps) {
                // Match the established shortcut policy: ordinary Ctrl/Alt name
                // the key; physical AltGr uses a third-level character if mapped.
                Err("layout-unmapped" | "control-character") => {
                    self.character(stroke.vk, shift, caps)
                }
                result => result,
            }
        } else {
            self.character(stroke.vk, shift, caps)
        }?;
        Ok(if value <= 0xff {
            value
        } else {
            0x0100_0000 | value
        })
    }
}

fn cache() -> &'static RwLock<HashMap<isize, Arc<Layout>>> {
    static CACHE: OnceLock<RwLock<HashMap<isize, Arc<Layout>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn supports(layout: isize) -> bool {
    cfg!(target_pointer_width = "64") && matches!(layout, 0x0407_0407 | 0x0409_0409 | 0x0804_0804)
}

// A local layout switch may leave the same hook armed. Prepare every already
// installed supported HKL so that switching does not need IO on the first key.
pub(crate) fn prepare_loaded(foreground: isize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayoutList, HKL};
    let mut layouts = [HKL::default(); 64];
    let count = unsafe { GetKeyboardLayoutList(Some(&mut layouts)) };
    for layout in std::iter::once(foreground).chain(
        layouts[..(count.max(0) as usize).min(layouts.len())]
            .iter()
            .map(|h| h.0 as isize),
    ) {
        if supports(layout) {
            if let Err(error) = prepare(layout) {
                eprintln!("VNC shortcut layout {layout:x} preparation failed: {error}");
            }
        }
    }
}

pub(crate) fn prepare(layout: isize) -> Result<(), Failure> {
    if cache()
        .read()
        .map_err(|_| "layout-cache-poisoned")?
        .contains_key(&layout)
    {
        return Ok(());
    }
    // No cache lock across registry reads, loader work, or native table copying.
    let owned = Arc::new(load(layout)?);
    cache()
        .write()
        .map_err(|_| "layout-cache-poisoned")?
        .entry(layout)
        .or_insert(owned);
    Ok(())
}

pub(crate) fn translate(stroke: &Stroke) -> Result<u32, Failure> {
    if stroke.vk == 0x10 && stroke.scan == 0x36 {
        return Ok(0xffe2);
    }
    if let Some(symbol) = super::vnc_chord::named_symbol(stroke.vk, stroke.extended) {
        return Ok(symbol);
    }
    let layout = cache()
        .read()
        .map_err(|_| "layout-cache-poisoned")?
        .get(&stroke.layout)
        .cloned()
        .ok_or("layout-not-prepared")?;
    layout.translate(stroke)
}

#[cfg(not(all(windows, target_pointer_width = "64")))]
fn load(_layout: isize) -> Result<Layout, Failure> {
    Err("layout-native-64-bit-required")
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn load(layout: isize) -> Result<Layout, Failure> {
    native::load(layout)
}

#[cfg(all(windows, target_pointer_width = "64"))]
mod native {
    use super::*;
    use std::ffi::c_void;
    use std::mem::{offset_of, size_of};
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::{FreeLibrary, ERROR_SUCCESS, HMODULE};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    use windows::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READ,
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE,
        PAGE_WRITECOPY,
    };
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{MODIFIERS, VK_TO_BIT, VK_TO_WCHAR_TABLE};

    // The prefix is sufficient. Older table versions need not include the later
    // dwType/dwSubType fields in windows-rs' full KBDTABLES declaration.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Tables {
        modifiers: *const MODIFIERS,
        rows: *const VK_TO_WCHAR_TABLE,
        dead: *const c_void,
        names: *const c_void,
        names_ext: *const c_void,
        names_dead: *const c_void,
        scans: *const u16,
        max_scan: u8,
        scans_e0: *const c_void,
        scans_e1: *const c_void,
        locale_flags: u32,
        max_ligature: u8,
        ligature_size: u8,
        ligatures: *const c_void,
    }

    struct Library(HMODULE);
    impl Drop for Library {
        fn drop(&mut self) {
            unsafe {
                let _ = FreeLibrary(self.0);
            }
        }
    }

    impl Library {
        fn range(&self, address: usize, length: usize) -> Result<(), Failure> {
            if address == 0 {
                return Err("layout-null-pointer");
            }
            let end = address
                .checked_add(length)
                .ok_or("layout-pointer-overflow")?;
            let mut next = address;
            while next < end {
                let mut info = MEMORY_BASIC_INFORMATION::default();
                let count = unsafe {
                    VirtualQuery(
                        Some(next as *const c_void),
                        &mut info,
                        size_of::<MEMORY_BASIC_INFORMATION>(),
                    )
                };
                let readable = PAGE_READONLY.0
                    | PAGE_READWRITE.0
                    | PAGE_WRITECOPY.0
                    | PAGE_EXECUTE_READ.0
                    | PAGE_EXECUTE_READWRITE.0
                    | PAGE_EXECUTE_WRITECOPY.0;
                if count != size_of::<MEMORY_BASIC_INFORMATION>()
                    || info.AllocationBase != self.0 .0
                    || info.State != MEM_COMMIT
                    || info.Protect.0 & PAGE_GUARD.0 != 0
                    || info.Protect.0 & readable == 0
                {
                    return Err("layout-pointer-outside-image");
                }
                let region_end = (info.BaseAddress as usize)
                    .checked_add(info.RegionSize)
                    .ok_or("layout-pointer-overflow")?;
                if region_end <= next {
                    return Err("layout-invalid-region");
                }
                next = region_end.min(end);
            }
            Ok(())
        }

        fn read<T: Copy>(&self, address: usize) -> Result<T, Failure> {
            self.range(address, size_of::<T>())?;
            Ok(unsafe { (address as *const T).read_unaligned() })
        }

        fn offset<T: Copy>(&self, address: usize, offset: usize) -> Result<T, Failure> {
            self.read(
                address
                    .checked_add(offset)
                    .ok_or("layout-pointer-overflow")?,
            )
        }
    }

    fn filename(klid: &str, expected: &str) -> Result<(), Failure> {
        let key: Vec<u16> =
            format!("SYSTEM\\CurrentControlSet\\Control\\Keyboard Layouts\\{klid}\0")
                .encode_utf16()
                .collect();
        let name: Vec<u16> = "Layout File\0".encode_utf16().collect();
        let mut value = [0u16; 260];
        let mut bytes = size_of::<[u16; 260]>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(value.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status != ERROR_SUCCESS
            || bytes < 2
            || bytes as usize > size_of::<[u16; 260]>()
            || !bytes.is_multiple_of(2)
        {
            return Err("layout-registry-unavailable");
        }
        let value = &value[..bytes as usize / 2];
        let end = value
            .iter()
            .position(|c| *c == 0)
            .ok_or("layout-registry-invalid")?;
        if value[end..].iter().any(|c| *c != 0) {
            return Err("layout-registry-invalid");
        }
        let actual = String::from_utf16(&value[..end]).map_err(|_| "layout-registry-invalid")?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err("layout-custom-backend-unsupported");
        }
        Ok(())
    }

    pub(super) fn load(layout: isize) -> Result<Layout, Failure> {
        // Do not infer a custom HKL's KLID from its language ID. These exact
        // standard identities are the only ones this initial backend recognizes.
        let (klid, dll) = match layout {
            0x0407_0407 => ("00000407", "KBDGR.DLL"),
            0x0409_0409 => ("00000409", "KBDUS.DLL"),
            0x0804_0804 => ("00000804", "KBDUS.DLL"),
            _ => return Err("layout-unsupported-hkl"),
        };
        filename(klid, dll)?;
        let mut path = vec![0u16; 32768];
        let length = unsafe { GetSystemDirectoryW(Some(&mut path)) } as usize;
        if length == 0 || length >= path.len() {
            return Err("layout-system-directory-unavailable");
        }
        path.truncate(length);
        path.extend(format!("\\{dll}\0").encode_utf16());
        let library = Library(
            unsafe { LoadLibraryExW(PCWSTR(path.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .map_err(|_| "layout-dll-unavailable")?,
        );
        if unsafe { GetProcAddress(library.0, PCSTR(c"KbdNlsLayerDescriptor".as_ptr().cast())) }
            .is_some()
        {
            return Err("layout-nls-unsupported");
        }
        let descriptor =
            unsafe { GetProcAddress(library.0, PCSTR(c"KbdLayerDescriptor".as_ptr().cast())) }
                .ok_or("layout-descriptor-unavailable")?;
        library.range(descriptor as usize, 1)?;
        let descriptor: unsafe extern "system" fn() -> *const Tables =
            unsafe { std::mem::transmute(descriptor) };
        let address = unsafe { descriptor() } as usize;
        let tables: Tables = library.read(address)?;
        if tables.locale_flags & 0xffff & !1 != 0 || tables.locale_flags >> 16 > 1 {
            return Err("layout-locale-flags-unsupported");
        }
        if tables.max_ligature != 0 || tables.ligature_size != 0 || !tables.ligatures.is_null() {
            return Err("layout-ligature-tables-unsupported");
        }
        let modifiers: MODIFIERS = library.read(tables.modifiers as usize)?;
        if modifiers.wMaxModBits > 7 {
            return Err("layout-modifiers-unsupported");
        }
        let mut bits_terminated = false;
        let mut seen = 0u8;
        for index in 0..4 {
            let entry: VK_TO_BIT =
                library.offset(modifiers.pVkToBit as usize, index * size_of::<VK_TO_BIT>())?;
            if entry.Vk == 0 {
                bits_terminated = true;
                break;
            }
            let expected = match entry.Vk {
                0x10 => 1,
                0x11 => 2,
                0x12 => 4,
                _ => return Err("layout-modifiers-unsupported"),
            };
            if entry.ModBits != expected || seen & expected != 0 {
                return Err("layout-modifiers-unsupported");
            }
            seen |= expected;
        }
        if !bits_terminated || seen != 7 {
            return Err("layout-modifiers-unsupported");
        }
        let mut modifier_columns = Vec::new();
        for index in 0..=usize::from(modifiers.wMaxModBits) {
            modifier_columns.push(library.offset::<u8>(
                tables.modifiers as usize,
                offset_of!(MODIFIERS, ModNumber) + index,
            )?);
        }
        let mut rows = vec![None; 256];
        let mut tables_terminated = false;
        for table_index in 0..32 {
            let table: VK_TO_WCHAR_TABLE = library.offset(
                tables.rows as usize,
                table_index * size_of::<VK_TO_WCHAR_TABLE>(),
            )?;
            if table.pVkToWchars.is_null() {
                tables_terminated = true;
                break;
            }
            let columns = usize::from(table.nModifications);
            let stride = usize::from(table.cbSize);
            if columns == 0 || columns > 8 || stride != 2 + 2 * columns {
                return Err("layout-row-shape-unsupported");
            }
            let mut rows_terminated = false;
            for row_index in 0..512 {
                let start = (table.pVkToWchars as usize)
                    .checked_add(row_index * stride)
                    .ok_or("layout-pointer-overflow")?;
                library.range(start, stride)?;
                let vk: u8 = library.read(start)?;
                if vk == 0 {
                    rows_terminated = true;
                    break;
                }
                if vk == 0xff {
                    continue;
                } // Dead-key continuation, never a physical VK.
                let attributes: u8 = library.offset(start, 1)?;
                let mut characters = Vec::with_capacity(columns);
                for column in 0..columns {
                    characters.push(library.offset::<u16>(start, 2 + 2 * column)?);
                }
                let row = Row {
                    attributes,
                    characters,
                };
                if rows[usize::from(vk)].is_some() {
                    return Err("layout-duplicate-vk-unsupported");
                }
                rows[usize::from(vk)] = Some(row);
            }
            if !rows_terminated {
                return Err("layout-row-limit");
            }
        }
        if !tables_terminated {
            return Err("layout-table-limit");
        }
        Ok(Layout {
            modifier_columns,
            rows,
        })
        // `library` drops here, including on every error. The cache owns no DLL
        // pointers, so translation cannot outlive a mapped native structure.
    }
}

#[cfg(test)]
mod tests {
    use super::super::vnc_chord::Modifiers;
    use super::*;

    fn german() -> Layout {
        let mut rows = vec![None; 256];
        for (vk, characters, attributes) in [
            (0x52, vec!['r' as u16, 'R' as u16], CAPLOK),
            (0x45, vec!['e' as u16, 'E' as u16, 0x20ac], CAPLOK),
            (0x51, vec!['q' as u16, 'Q' as u16, '@' as u16], CAPLOK),
            (0x31, vec!['1' as u16, '!' as u16], CAPLOK),
            (0xdd, vec![DEAD, DEAD], 0),
            (0xc0, vec![LIGATURE, NONE], 0),
            (0xba, vec!['a' as u16, 'A' as u16], 2),
        ] {
            rows[vk] = Some(Row {
                attributes,
                characters,
            });
        }
        Layout {
            modifier_columns: vec![0, 1, 3, 4, 0xf, 0xf, 2],
            rows,
        }
    }

    fn stroke(vk: u16, shift: bool, caps: bool, altgr: bool) -> Stroke {
        Stroke {
            vk,
            scan: 0,
            extended: false,
            pressed: true,
            repeat: false,
            layout: 0x0407_0407,
            modifiers: Modifiers {
                shift,
                caps: Some(caps),
                control: altgr,
                alt: altgr,
                right_alt: altgr,
            },
        }
    }

    #[test]
    fn per_row_caps_shift_and_altgr_choose_real_columns() {
        let map = german();
        for (shift, caps, expected) in [
            (false, false, 'r'),
            (true, false, 'R'),
            (false, true, 'R'),
            (true, true, 'r'),
        ] {
            assert_eq!(
                map.translate(&stroke(0x52, shift, caps, false)),
                Ok(expected as u32)
            );
        }
        assert_eq!(
            map.translate(&stroke(0x31, false, true, false)),
            Ok('!' as u32)
        );
        assert_eq!(
            map.translate(&stroke(0x31, true, true, false)),
            Ok('1' as u32)
        );
        for caps in [false, true] {
            assert_eq!(
                map.translate(&stroke(0x51, false, caps, true)),
                Ok('@' as u32)
            );
            assert_eq!(
                map.translate(&stroke(0x45, false, caps, true)),
                Ok(0x0100_20ac)
            );
        }
        assert_eq!(
            map.translate(&stroke(0x52, false, true, true)),
            Ok('R' as u32)
        );
        let mut ctrl = stroke(0x52, false, false, false);
        ctrl.modifiers.control = true;
        assert_eq!(map.translate(&ctrl), Ok('r' as u32));
    }

    #[test]
    fn unsupported_table_semantics_are_never_guessed() {
        let map = german();
        assert_eq!(
            map.translate(&stroke(0xdd, false, false, false)),
            Err("layout-dead-key")
        );
        assert_eq!(
            map.translate(&stroke(0xc0, false, false, false)),
            Err("layout-ligature")
        );
        assert_eq!(
            map.translate(&stroke(0xba, false, false, false)),
            Err("layout-unsupported-attributes")
        );
        let mut unknown_caps = stroke(0x52, false, false, false);
        unknown_caps.modifiers.caps = None;
        assert_eq!(map.translate(&unknown_caps), Err("caps-state-unavailable"));
    }

    #[cfg(all(
        windows,
        target_pointer_width = "64",
        feature = "native-vnc-acceptance"
    ))]
    #[test]
    #[ignore = "isolated VM; ADIT_VNC_ISOLATED_LAYOUT_TEST=1 and preloaded standard German required"]
    fn native_owned_layout_ignores_pending_accent_without_consuming_it() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetKeyboardLayoutList, ToUnicodeEx, HKL,
        };
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_NOREMOVE};
        assert_eq!(
            std::env::var("ADIT_VNC_ISOLATED_LAYOUT_TEST").as_deref(),
            Ok("1"),
            "unsupported: isolated VM required"
        );
        let mut layouts = [HKL::default(); 64];
        let count = unsafe { GetKeyboardLayoutList(Some(&mut layouts)) };
        assert!(count > 0, "unsupported: cannot enumerate loaded layouts");
        assert!(
            layouts[..(count as usize).min(layouts.len())]
                .iter()
                .any(|h| h.0 as usize == 0x0407_0407),
            "unsupported: German must already be loaded"
        );
        let result = std::thread::spawn(|| {
            let mut message = MSG::default();
            unsafe {
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            }
            let state = [0u8; 256];
            let mut text = [0u16; 16];
            let hkl = HKL(0x0407_0407 as *mut _);
            let accent = unsafe { ToUnicodeEx(0xdd, 0x0d, &state, &mut text, 0, Some(hkl)) };
            // Deliberately prepare during the pending accent, not just before it.
            // No assertion/early return may skip consuming this test queue's accent.
            let loaded = prepare(0x0407_0407);
            let lookup = |vk, caps, altgr| {
                loaded.as_ref().map_err(|error| *error).and_then(|_| {
                    super::super::vnc_chord::translate(&stroke(vk, false, caps, altgr))
                })
            };
            let r = lookup(0x52, false, false);
            let e = lookup(0x45, false, false);
            let euro = lookup(0x45, true, true);
            let prepared = loaded.map(|_| ());
            text.fill(0);
            let composed = unsafe { ToUnicodeEx(0x45, 0x12, &state, &mut text, 0, Some(hkl)) };
            (accent, prepared, r, e, euro, composed, text)
        })
        .join()
        .expect("test composition thread completes");
        assert!(result.0 < 0, "acute must establish a real pending accent");
        assert_eq!(result.1, Ok(()));
        assert_eq!(result.2, Ok('r' as u32));
        assert_eq!(result.3, Ok('e' as u32));
        assert_eq!(result.4, Ok(0x0100_20ac));
        assert_eq!(result.5, 1);
        assert_eq!(result.6[0], 'é' as u16);
    }
}
