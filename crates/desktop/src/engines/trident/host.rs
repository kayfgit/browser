//! The Trident helper process (`browser.exe --trident-host`): hosts one MSHTML
//! WebBrowser control in its own window, which the browser places in a pane.
//!
//! It runs sandboxed (see `sandbox`) and takes [`Command`]s on stdin, answering with
//! [`Event`]s on stdout. The control needs an OLE container, so this is one: a client
//! site, an in-place site and frame, and `IDocHostUIHandler`, which is how MSHTML asks
//! its host about context menus, accelerators and `window.external`.
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::c_void,
    io::{BufRead, Write},
    mem::ManuallyDrop,
    rc::Rc,
    sync::{Arc, Mutex},
};

use windows::{
    core::{
        implement, w, IUnknown, Interface, OutRef, Ref, BOOL, BSTR, GUID, HRESULT, HSTRING, PCWSTR,
        PWSTR,
    },
    Win32::{
        Foundation::{
            DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_NOINTERFACE, E_NOTIMPL, HWND, LPARAM,
            LRESULT, POINT, RECT, SIZE, S_FALSE, S_OK, VARIANT_BOOL, VARIANT_TRUE, WPARAM,
        },
        System::{
            Com::{
                CoCreateInstance, IConnectionPointContainer, IDispatch, IDispatch_Impl,
                IEnumString, IMoniker, IServiceProvider, IServiceProvider_Impl, ITypeInfo,
                Urlmon::{
                    IInternetSecurityManager, IInternetSecurityManager_Impl,
                    IInternetSecurityMgrSite, INET_E_DEFAULT_ACTION,
                    URLACTION_ACTIVEX_OVERRIDE_OBJECT_SAFETY, URLACTION_ACTIVEX_OVERRIDE_OPTIN,
                    URLACTION_ACTIVEX_TREATASUNTRUSTED, URLACTION_DOWNLOAD_SIGNED_ACTIVEX,
                    URLACTION_DOWNLOAD_UNSIGNED_ACTIVEX, URLACTION_JAVA_MAX, URLACTION_JAVA_MIN,
                    URLACTION_SHELL_FILE_DOWNLOAD, URLPOLICY_DISALLOW,
                },
                CLSCTX_INPROC_SERVER, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET,
                DISPPARAMS, EXCEPINFO,
            },
            LibraryLoader::GetModuleHandleW,
            Ole::{
                IOleClientSite, IOleClientSite_Impl, IOleContainer, IOleInPlaceActiveObject,
                IOleInPlaceFrame, IOleInPlaceFrame_Impl, IOleInPlaceObject, IOleInPlaceSite,
                IOleInPlaceSite_Impl, IOleInPlaceUIWindow, IOleInPlaceUIWindow_Impl, IOleObject,
                IOleWindow_Impl, OleInitialize, OLECMDEXECOPT_DONTPROMPTUSER, OLECMDID,
                OLEGETMONIKER, OLEINPLACEFRAMEINFO, OLEIVERB_INPLACEACTIVATE, OLEIVERB_UIACTIVATE,
                OLEMENUGROUPWIDTHS, OLEWHICHMK,
            },
            Variant::{
                VariantClear, VARIANT, VT_BOOL, VT_BSTR, VT_BYREF, VT_DISPATCH, VT_I4, VT_VARIANT,
            },
        },
        UI::{
            Shell::{DWebBrowserEvents2, IWebBrowser2, WebBrowser},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowExW, GetClientRect,
                GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
                CS_HREDRAW, CS_VREDRAW, HMENU, MSG, WINDOW_EX_STYLE, WM_APP, WM_DESTROY,
                WM_KEYFIRST, WM_KEYLAST, WM_SIZE, WNDCLASSW, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
                WS_POPUP,
            },
        },
    },
};

use super::protocol::{self, Command, Event};

/// `WM_APP + 1`: commands are waiting in the queue.
const WM_COMMANDS: u32 = WM_APP + 1;

// DWebBrowserEvents2 dispatch IDs (exdispid.h).
const DISPID_COMMANDSTATECHANGE: i32 = 105;
const DISPID_BEFORENAVIGATE2: i32 = 250;
const DISPID_NAVIGATECOMPLETE2: i32 = 252;
const DISPID_DOCUMENTCOMPLETE: i32 = 259;
const DISPID_WINDOWCLOSING: i32 = 263;
const DISPID_FILEDOWNLOAD: i32 = 270;
const DISPID_NAVIGATEERROR: i32 = 271;
const DISPID_NEWWINDOW3: i32 = 273;
// CommandStateChange's commands (CSC_NAVIGATEFORWARD / CSC_NAVIGATEBACK).
const CSC_NAVIGATEFORWARD: i32 = 1;
const CSC_NAVIGATEBACK: i32 = 2;
/// `OLECMDID_OPTICAL_ZOOM`: page zoom in percent.
const OLECMDID_OPTICAL_ZOOM: OLECMDID = OLECMDID(63);

// DOCHOSTUIINFO flags: no 3D borders, themed controls, DPI-aware rendering.
const DOCHOSTUIFLAG_NO3DBORDER: u32 = 0x4;
const DOCHOSTUIFLAG_THEME: u32 = 0x40000;
const DOCHOSTUIFLAG_NO3DOUTERBORDER: u32 = 0x200000;
const DOCHOSTUIFLAG_DPI_AWARE: u32 = 0x40000000;

#[allow(non_snake_case)]
mod mshtml {
    use super::DocHostUiInfo;
    use std::ffi::c_void;
    use windows::core::{interface, IUnknown, IUnknown_Vtbl, BOOL, GUID, HRESULT, PCWSTR, PWSTR};
    use windows::Win32::{
        Foundation::{POINT, RECT},
        UI::WindowsAndMessaging::MSG,
    };

    /// What MSHTML asks its host (mshtmhst.h). `windows` doesn't carry the MSHTML
    /// interfaces, so it's declared here, in vtable order.
    #[interface("bd3f23c0-d43e-11cf-893b-00aa00bdce1a")]
    pub(crate) unsafe trait IDocHostUIHandler: IUnknown {
        unsafe fn ShowContextMenu(
            &self,
            id: u32,
            pt: *const POINT,
            target: *mut c_void,
            object: *mut c_void,
        ) -> HRESULT;
        unsafe fn GetHostInfo(&self, info: *mut DocHostUiInfo) -> HRESULT;
        unsafe fn ShowUI(
            &self,
            id: u32,
            active: *mut c_void,
            target: *mut c_void,
            frame: *mut c_void,
            doc: *mut c_void,
        ) -> HRESULT;
        unsafe fn HideUI(&self) -> HRESULT;
        unsafe fn UpdateUI(&self) -> HRESULT;
        unsafe fn EnableModeless(&self, enable: BOOL) -> HRESULT;
        unsafe fn OnDocWindowActivate(&self, activate: BOOL) -> HRESULT;
        unsafe fn OnFrameWindowActivate(&self, activate: BOOL) -> HRESULT;
        unsafe fn ResizeBorder(
            &self,
            border: *const RECT,
            window: *mut c_void,
            frame: BOOL,
        ) -> HRESULT;
        unsafe fn TranslateAccelerator(
            &self,
            msg: *const MSG,
            group: *const GUID,
            cmd: u32,
        ) -> HRESULT;
        unsafe fn GetOptionKeyPath(&self, key: *mut PWSTR, reserved: u32) -> HRESULT;
        unsafe fn GetDropTarget(&self, target: *mut c_void, out: *mut *mut c_void) -> HRESULT;
        unsafe fn GetExternal(&self, out: *mut *mut c_void) -> HRESULT;
        unsafe fn TranslateUrl(&self, translate: u32, url: PCWSTR, out: *mut PWSTR) -> HRESULT;
        unsafe fn FilterDataObject(&self, object: *mut c_void, out: *mut *mut c_void) -> HRESULT;
    }
}
use mshtml::{IDocHostUIHandler, IDocHostUIHandler_Impl};

#[repr(C)]
pub(crate) struct DocHostUiInfo {
    size: u32,
    pub(crate) flags: u32,
    pub(crate) double_click: u32,
    host_css: PWSTR,
    host_ns: PWSTR,
}

/// The helper's state, shared by its window procedure and COM objects (one thread).
struct Host {
    hwnd: HWND,
    browser: RefCell<Option<IWebBrowser2>>,
    /// Scripts for every top-level document (`Command::Init`).
    init: RefCell<String>,
    back: Cell<bool>,
    forward: Cell<bool>,
    /// The page zoom in percent, to turn CSS pixels into window pixels.
    zoom: Cell<i32>,
    commands: Arc<Mutex<VecDeque<Command>>>,
}

thread_local! {
    static HOST: RefCell<Option<Rc<Host>>> = const { RefCell::new(None) };
}

impl Host {
    fn emit(&self, event: &Event) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(protocol::encode(event).as_bytes());
        let _ = out.flush();
    }

    fn browser(&self) -> Option<IWebBrowser2> {
        self.browser.borrow().clone()
    }

    /// Whether `disp` (an event's subject) is the top-level browser, not a frame.
    fn is_top(&self, disp: Option<&IDispatch>) -> bool {
        let (Some(disp), Some(browser)) = (disp, self.browser()) else {
            return false;
        };
        let a = disp.cast::<IUnknown>().ok();
        let b = browser.cast::<IUnknown>().ok();
        a.is_some() && a.map(|a| a.as_raw()) == b.map(|b| b.as_raw())
    }

    fn run(&self, command: Command) {
        let Some(browser) = self.browser() else {
            return;
        };
        let name: String = format!("{command:?}")
            .chars()
            .take_while(|c| c.is_alphanumeric())
            .collect();
        let result = unsafe {
            match command {
                Command::Init { script } => {
                    *self.init.borrow_mut() = script;
                    Ok(())
                }
                Command::Navigate { url } => {
                    let target = variant_str(&url);
                    browser.Navigate2(&*target, None, None, None, None)
                }
                Command::Reload => browser.Refresh(),
                Command::Go { forward } => {
                    if forward {
                        browser.GoForward()
                    } else {
                        browser.GoBack()
                    }
                }
                Command::Zoom { percent } => {
                    self.zoom.set(percent);
                    // Refused while no document is loaded yet; `DocumentComplete` applies it.
                    self.apply_zoom(&browser);
                    Ok(())
                }
                Command::Focus => self.focus(&browser),
                Command::Eval { script } => self.eval(&script),
                Command::Click { x, y } => {
                    self.click(x, y);
                    Ok(())
                }
                Command::Quit => {
                    PostQuitMessage(0);
                    Ok(())
                }
            }
        };
        if let Err(e) = result {
            self.emit(&Event::Notice {
                text: format!("Trident {name}: {} ({:#x})", e.message(), e.code().0),
            });
        }
    }

    /// UI-activate the control and put the keyboard in its document window.
    unsafe fn focus(&self, browser: &IWebBrowser2) -> windows::core::Result<()> {
        let object: IOleObject = browser.cast()?;
        let site = object.GetClientSite()?;
        let mut rect = RECT::default();
        let _ = GetClientRect(self.hwnd, &mut rect);
        object.DoVerb(
            OLEIVERB_UIACTIVATE.0,
            std::ptr::null(),
            &site,
            0,
            self.hwnd,
            &rect,
        )?;
        if let Some(view) = document_window(self.hwnd) {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(view));
        }
        Ok(())
    }

    /// Apply the requested page zoom. MSHTML only zooms a loaded document, so this
    /// runs again for every new one.
    fn apply_zoom(&self, browser: &IWebBrowser2) {
        let level = variant_i4(self.zoom.get());
        unsafe {
            let _ = browser.ExecWB(
                OLECMDID_OPTICAL_ZOOM,
                OLECMDEXECOPT_DONTPROMPTUSER,
                Some(&*level),
                None,
            );
        }
    }

    /// Click at (`x`, `y`) CSS pixels: posted mouse input, which the page sees as real.
    fn click(&self, x: f64, y: f64) {
        use windows::Win32::UI::HiDpi::GetDpiForWindow;
        use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE};
        let Some(view) = document_window(self.hwnd) else {
            return;
        };
        let scale = unsafe { GetDpiForWindow(view) } as f64 / 96.0 * self.zoom.get() as f64 / 100.0;
        let (px, py) = ((x * scale).round() as i32, (y * scale).round() as i32);
        let at = LPARAM(((py as u32 as isize & 0xffff) << 16) | (px as u32 as isize & 0xffff));
        unsafe {
            let _ = PostMessageW(Some(view), WM_MOUSEMOVE, WPARAM(0), at);
            let _ = PostMessageW(Some(view), WM_LBUTTONDOWN, WPARAM(1), at);
            let _ = PostMessageW(Some(view), WM_LBUTTONUP, WPARAM(0), at);
        }
    }

    /// Run `script` in the top-level document.
    unsafe fn eval(&self, script: &str) -> windows::core::Result<()> {
        let Some(browser) = self.browser() else {
            return Ok(());
        };
        let document = browser.Document()?;
        let window = get(&document, "parentWindow")?;
        let window =
            dispatch_of(&window).ok_or_else(|| windows::core::Error::from(E_NOINTERFACE))?;
        // execScript runs in the page's global scope; IE11's standards mode hides it
        // from page scripts but not from the host.
        call(
            &window,
            "execScript",
            &[variant_str(script), variant_str("JavaScript")],
        )
        .map(|_| ())
    }

    /// A new top-level document: give it the scripts (they guard against running twice).
    fn inject(&self) {
        let script = self.init.borrow().clone();
        if !script.is_empty() {
            unsafe {
                let _ = self.eval(&script);
            }
        }
    }

    fn history(&self, command: i32, enabled: bool) {
        match command {
            CSC_NAVIGATEBACK => self.back.set(enabled),
            CSC_NAVIGATEFORWARD => self.forward.set(enabled),
            _ => return,
        }
        self.emit(&Event::History {
            back: self.back.get(),
            forward: self.forward.get(),
        });
    }

    fn url(&self) {
        if let Some(browser) = self.browser() {
            if let Ok(url) = unsafe { browser.LocationURL() } {
                self.emit(&Event::Url {
                    url: url.to_string(),
                });
            }
        }
    }
}

/// The control's document window (`Internet Explorer_Server`), deep in its children.
fn document_window(hwnd: HWND) -> Option<HWND> {
    let mut current = hwnd;
    for class in [
        w!("Shell Embedding"),
        w!("Shell DocObject View"),
        w!("Internet Explorer_Server"),
    ] {
        current = unsafe { FindWindowExW(Some(current), None, class, None) }.ok()?;
    }
    Some(current)
}

// --- VARIANT helpers -------------------------------------------------------------

/// A VARIANT that clears itself.
struct Var(VARIANT);
impl Drop for Var {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}
impl std::ops::Deref for Var {
    type Target = VARIANT;
    fn deref(&self) -> &VARIANT {
        &self.0
    }
}

fn variant_str(s: &str) -> Var {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BSTR;
        inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
    }
    Var(v)
}

fn variant_i4(n: i32) -> Var {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_I4;
        inner.Anonymous.lVal = n;
    }
    Var(v)
}

/// The string in `v`, following by-reference variants (how events pass URLs).
fn string_of(v: &VARIANT) -> Option<String> {
    unsafe {
        let inner = &*v.Anonymous.Anonymous;
        if inner.vt == VT_BSTR {
            return Some(inner.Anonymous.bstrVal.to_string());
        }
        if inner.vt == VARENUM_BYREF_VARIANT && !inner.Anonymous.pvarVal.is_null() {
            return string_of(&*inner.Anonymous.pvarVal);
        }
        if inner.vt.0 == VT_BYREF.0 | VT_BSTR.0 && !inner.Anonymous.pbstrVal.is_null() {
            return Some((*inner.Anonymous.pbstrVal).to_string());
        }
    }
    None
}

const VARENUM_BYREF_VARIANT: windows::Win32::System::Variant::VARENUM =
    windows::Win32::System::Variant::VARENUM(VT_BYREF.0 | VT_VARIANT.0);

fn i32_of(v: &VARIANT) -> Option<i32> {
    unsafe {
        let inner = &*v.Anonymous.Anonymous;
        (inner.vt == VT_I4).then_some(inner.Anonymous.lVal)
    }
}

fn bool_of(v: &VARIANT) -> Option<bool> {
    unsafe {
        let inner = &*v.Anonymous.Anonymous;
        (inner.vt == VT_BOOL).then(|| inner.Anonymous.boolVal.as_bool())
    }
}

fn dispatch_of(v: &VARIANT) -> Option<IDispatch> {
    unsafe {
        let inner = &*v.Anonymous.Anonymous;
        if inner.vt == VT_DISPATCH {
            return (*inner.Anonymous.pdispVal).clone();
        }
        if inner.vt == VARENUM_BYREF_VARIANT && !inner.Anonymous.pvarVal.is_null() {
            return dispatch_of(&*inner.Anonymous.pvarVal);
        }
    }
    None
}

/// Set a by-reference `VARIANT_BOOL` out-parameter (an event's `Cancel`).
fn set_cancel(v: &VARIANT) {
    unsafe {
        let inner = &*v.Anonymous.Anonymous;
        if inner.vt.0 == VT_BYREF.0 | VT_BOOL.0 && !inner.Anonymous.pboolVal.is_null() {
            *inner.Anonymous.pboolVal = VARIANT_TRUE;
        }
    }
}

/// An event's arguments in declaration order (DISPPARAMS holds them reversed).
fn args(params: *const DISPPARAMS) -> Vec<&'static VARIANT> {
    if params.is_null() {
        return Vec::new();
    }
    let params = unsafe { &*params };
    let n = params.cArgs as usize;
    if n == 0 || params.rgvarg.is_null() {
        return Vec::new();
    }
    let all = unsafe { std::slice::from_raw_parts(params.rgvarg, n) };
    all.iter().rev().collect()
}

// --- late-bound calls ------------------------------------------------------------

fn dispid(object: &IDispatch, name: &str) -> windows::core::Result<i32> {
    let name = HSTRING::from(name);
    let names = [PCWSTR(name.as_ptr())];
    let mut id = 0;
    unsafe { object.GetIDsOfNames(&GUID::zeroed(), names.as_ptr(), 1, 0x0400, &mut id)? };
    Ok(id)
}

fn get(object: &IDispatch, name: &str) -> windows::core::Result<Var> {
    let id = dispid(object, name)?;
    let mut result = Var(VARIANT::default());
    unsafe {
        object.Invoke(
            id,
            &GUID::zeroed(),
            0x0400,
            DISPATCH_PROPERTYGET,
            &DISPPARAMS::default(),
            Some(&mut result.0),
            None,
            None,
        )?
    };
    Ok(result)
}

fn call(object: &IDispatch, name: &str, arguments: &[Var]) -> windows::core::Result<Var> {
    let id = dispid(object, name)?;
    // Reversed, as DISPPARAMS wants them. Shallow copies: `arguments` owns them.
    let mut reversed: Vec<VARIANT> = arguments
        .iter()
        .rev()
        .map(|v| unsafe { std::ptr::read(&v.0) })
        .collect();
    let params = DISPPARAMS {
        rgvarg: reversed.as_mut_ptr(),
        rgdispidNamedArgs: std::ptr::null_mut(),
        cArgs: reversed.len() as u32,
        cNamedArgs: 0,
    };
    let mut result = Var(VARIANT::default());
    let r = unsafe {
        object.Invoke(
            id,
            &GUID::zeroed(),
            0x0400,
            DISPATCH_METHOD,
            &params,
            Some(&mut result.0),
            None,
            None,
        )
    };
    // The copies alias `arguments`' contents; don't let both free them.
    std::mem::forget(reversed);
    r.map(|_| result)
}

// --- the OLE container -----------------------------------------------------------

#[implement(IOleClientSite, IOleInPlaceSite, IDocHostUIHandler, IServiceProvider)]
struct Site {
    host: Rc<Host>,
    frame: IOleInPlaceFrame,
    external: IDispatch,
    security: IInternetSecurityManager,
}

impl IOleClientSite_Impl for Site_Impl {
    fn SaveObject(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetMoniker(&self, _: &OLEGETMONIKER, _: &OLEWHICHMK) -> windows::core::Result<IMoniker> {
        Err(E_NOTIMPL.into())
    }
    fn GetContainer(&self) -> windows::core::Result<IOleContainer> {
        Err(E_NOINTERFACE.into())
    }
    fn ShowObject(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnShowWindow(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
    fn RequestNewObjectLayout(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

impl IOleWindow_Impl for Site_Impl {
    fn GetWindow(&self) -> windows::core::Result<HWND> {
        Ok(self.host.hwnd)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

impl IOleInPlaceSite_Impl for Site_Impl {
    fn CanInPlaceActivate(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnInPlaceActivate(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnUIActivate(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn GetWindowContext(
        &self,
        frame: OutRef<IOleInPlaceFrame>,
        doc: OutRef<IOleInPlaceUIWindow>,
        position: *mut RECT,
        clip: *mut RECT,
        info: *mut OLEINPLACEFRAMEINFO,
    ) -> windows::core::Result<()> {
        frame.write(Some(self.frame.clone()))?;
        doc.write(None)?;
        unsafe {
            let mut rect = RECT::default();
            let _ = GetClientRect(self.host.hwnd, &mut rect);
            if !position.is_null() {
                *position = rect;
            }
            if !clip.is_null() {
                *clip = rect;
            }
            if !info.is_null() {
                (*info).fMDIApp = false.into();
                (*info).hwndFrame = self.host.hwnd;
                (*info).haccel = Default::default();
                (*info).cAccelEntries = 0;
            }
        }
        Ok(())
    }
    fn Scroll(&self, _: &SIZE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnUIDeactivate(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnInPlaceDeactivate(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn DiscardUndoState(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn DeactivateAndUndo(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnPosRectChange(&self, _: *const RECT) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IDocHostUIHandler_Impl for Site_Impl {
    unsafe fn ShowContextMenu(
        &self,
        _: u32,
        _: *const POINT,
        _: *mut c_void,
        _: *mut c_void,
    ) -> HRESULT {
        // Never IE's own menu; the shell bridge draws ours from the DOM event.
        S_OK
    }
    unsafe fn GetHostInfo(&self, info: *mut DocHostUiInfo) -> HRESULT {
        if !info.is_null() {
            (*info).flags = DOCHOSTUIFLAG_NO3DBORDER
                | DOCHOSTUIFLAG_NO3DOUTERBORDER
                | DOCHOSTUIFLAG_THEME
                | DOCHOSTUIFLAG_DPI_AWARE;
            (*info).double_click = 0;
        }
        S_OK
    }
    unsafe fn ShowUI(
        &self,
        _: u32,
        _: *mut c_void,
        _: *mut c_void,
        _: *mut c_void,
        _: *mut c_void,
    ) -> HRESULT {
        S_OK
    }
    unsafe fn HideUI(&self) -> HRESULT {
        S_OK
    }
    unsafe fn UpdateUI(&self) -> HRESULT {
        S_OK
    }
    unsafe fn EnableModeless(&self, _: BOOL) -> HRESULT {
        S_OK
    }
    unsafe fn OnDocWindowActivate(&self, _: BOOL) -> HRESULT {
        S_OK
    }
    unsafe fn OnFrameWindowActivate(&self, _: BOOL) -> HRESULT {
        S_OK
    }
    unsafe fn ResizeBorder(&self, _: *const RECT, _: *mut c_void, _: BOOL) -> HRESULT {
        S_OK
    }
    unsafe fn TranslateAccelerator(&self, msg: *const MSG, _: *const GUID, _: u32) -> HRESULT {
        // IE's own commands that open windows or dialogs outside the pane: new
        // window, open, print, find, address bar. Everything else is the page's.
        if !msg.is_null() && blocked_accelerator(&*msg) {
            return S_OK;
        }
        S_FALSE
    }
    unsafe fn GetOptionKeyPath(&self, key: *mut PWSTR, _: u32) -> HRESULT {
        if !key.is_null() {
            *key = PWSTR::null();
        }
        S_FALSE
    }
    unsafe fn GetDropTarget(&self, _: *mut c_void, _: *mut *mut c_void) -> HRESULT {
        E_NOTIMPL
    }
    unsafe fn GetExternal(&self, out: *mut *mut c_void) -> HRESULT {
        if out.is_null() {
            return E_NOTIMPL;
        }
        *out = self.external.clone().into_raw();
        S_OK
    }
    unsafe fn TranslateUrl(&self, _: u32, _: PCWSTR, out: *mut PWSTR) -> HRESULT {
        if !out.is_null() {
            *out = PWSTR::null();
        }
        S_FALSE
    }
    unsafe fn FilterDataObject(&self, _: *mut c_void, out: *mut *mut c_void) -> HRESULT {
        if !out.is_null() {
            *out = std::ptr::null_mut();
        }
        S_FALSE
    }
}

impl IServiceProvider_Impl for Site_Impl {
    fn QueryService(
        &self,
        service: *const GUID,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if service.is_null() || iid.is_null() || out.is_null() {
            return Err(E_NOINTERFACE.into());
        }
        unsafe {
            *out = std::ptr::null_mut();
            if *service == SID_INTERNET_SECURITY_MANAGER && *iid == IInternetSecurityManager::IID {
                *out = self.security.clone().into_raw();
                return Ok(());
            }
        }
        Err(E_NOINTERFACE.into())
    }
}

/// `SID_SInternetSecurityManager`: how MSHTML finds a host's security manager.
const SID_INTERNET_SECURITY_MANAGER: GUID = GUID::from_u128(0x79eac9ee_baf9_11ce_8c82_00aa004ba90b);

/// The page's security policy, on top of IE's zone settings, whatever zone a page is
/// in: no ActiveX controls that aren't marked safe, no installing controls, no Java and
/// no IE downloads. Everything else is IE's default, so old sites' safe built-in
/// controls (`new ActiveXObject("Microsoft.XMLHTTP")`) keep working.
#[implement(IInternetSecurityManager)]
struct Security;

impl IInternetSecurityManager_Impl for Security_Impl {
    fn SetSecuritySite(&self, _: Ref<IInternetSecurityMgrSite>) -> windows::core::Result<()> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
    fn GetSecuritySite(&self) -> windows::core::Result<IInternetSecurityMgrSite> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
    fn MapUrlToZone(&self, _: &PCWSTR, _: *mut u32, _: u32) -> windows::core::Result<()> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
    fn GetSecurityId(
        &self,
        _: &PCWSTR,
        _: *mut u8,
        _: *mut u32,
        _: usize,
    ) -> windows::core::Result<()> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
    fn ProcessUrlAction(
        &self,
        _: &PCWSTR,
        action: u32,
        policy: *mut u8,
        size: u32,
        _: *const u8,
        _: u32,
        _: u32,
        _: u32,
    ) -> windows::core::Result<()> {
        if !forbidden_action(action) {
            return Err(HRESULT(INET_E_DEFAULT_ACTION).into());
        }
        if !policy.is_null() && size as usize >= std::mem::size_of::<u32>() {
            unsafe { policy.cast::<u32>().write_unaligned(URLPOLICY_DISALLOW) };
        }
        // S_FALSE: answered, and the answer is no.
        Err(S_FALSE.into())
    }
    fn QueryCustomPolicy(
        &self,
        _: &PCWSTR,
        _: *const GUID,
        _: *mut *mut u8,
        _: *mut u32,
        _: *const u8,
        _: u32,
        _: u32,
    ) -> windows::core::Result<()> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
    fn SetZoneMapping(&self, _: u32, _: &PCWSTR, _: u32) -> windows::core::Result<()> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
    fn GetZoneMappings(&self, _: u32, _: OutRef<IEnumString>, _: u32) -> windows::core::Result<()> {
        Err(HRESULT(INET_E_DEFAULT_ACTION).into())
    }
}

/// URL actions the policy always refuses: overriding a control's "safe for
/// scripting/initialization" marking (0x1201-0x1206), running controls never used
/// before without asking (0x1209), installing controls, Java, and IE's file downloads.
fn forbidden_action(action: u32) -> bool {
    (URLACTION_ACTIVEX_OVERRIDE_OBJECT_SAFETY..=URLACTION_ACTIVEX_TREATASUNTRUSTED)
        .contains(&action)
        || action == URLACTION_ACTIVEX_OVERRIDE_OPTIN
        || (URLACTION_JAVA_MIN..=URLACTION_JAVA_MAX).contains(&action)
        || action == URLACTION_SHELL_FILE_DOWNLOAD
        || action == URLACTION_DOWNLOAD_SIGNED_ACTIVEX
        || action == URLACTION_DOWNLOAD_UNSIGNED_ACTIVEX
}

/// Ctrl+N/O/P/F/L/E/H/I/J and F1: IE commands that would open windows or dialogs.
fn blocked_accelerator(msg: &MSG) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_F1};
    use windows::Win32::UI::WindowsAndMessaging::{WM_KEYDOWN, WM_SYSKEYDOWN};
    if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
        return false;
    }
    let key = msg.wParam.0 as u16;
    if key == VK_F1.0 {
        return true;
    }
    let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    ctrl && b"NOPFLEHIJ".contains(&(key as u8)) && key < 0x80
}

#[implement(IOleInPlaceFrame)]
struct Frame {
    hwnd: HWND,
}

impl IOleWindow_Impl for Frame_Impl {
    fn GetWindow(&self) -> windows::core::Result<HWND> {
        Ok(self.hwnd)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

impl IOleInPlaceUIWindow_Impl for Frame_Impl {
    fn GetBorder(&self) -> windows::core::Result<RECT> {
        Err(E_NOTIMPL.into())
    }
    fn RequestBorderSpace(&self, _: *const RECT) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetBorderSpace(&self, _: *const RECT) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetActiveObject(
        &self,
        _: Ref<IOleInPlaceActiveObject>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IOleInPlaceFrame_Impl for Frame_Impl {
    fn InsertMenus(&self, _: HMENU, _: *mut OLEMENUGROUPWIDTHS) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetMenu(&self, _: HMENU, _: isize, _: HWND) -> windows::core::Result<()> {
        Ok(())
    }
    fn RemoveMenus(&self, _: HMENU) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetStatusText(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn EnableModeless(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
    fn TranslateAccelerator(&self, _: *const MSG, _: u16) -> windows::core::Result<()> {
        Err(S_FALSE.into())
    }
}

/// `window.external`: `window.external.post(message)` reaches the shell.
#[implement(IDispatch)]
struct External {
    host: std::rc::Weak<Host>,
}

const DISPID_POST: i32 = 1;

impl IDispatch_Impl for External_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }
    fn GetTypeInfo(&self, _: u32, _: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(
        &self,
        _: *const GUID,
        names: *const PCWSTR,
        count: u32,
        _: u32,
        ids: *mut i32,
    ) -> windows::core::Result<()> {
        if count != 1 || names.is_null() || ids.is_null() {
            return Err(DISP_E_UNKNOWNNAME.into());
        }
        let name = unsafe { (*names).to_string() }.unwrap_or_default();
        if name != "post" {
            return Err(DISP_E_UNKNOWNNAME.into());
        }
        unsafe { *ids = DISPID_POST };
        Ok(())
    }
    fn Invoke(
        &self,
        id: i32,
        _: *const GUID,
        _: u32,
        _: DISPATCH_FLAGS,
        params: *const DISPPARAMS,
        _: *mut VARIANT,
        _: *mut EXCEPINFO,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        if id != DISPID_POST {
            return Err(DISP_E_MEMBERNOTFOUND.into());
        }
        let body = args(params).first().and_then(|v| string_of(v));
        if let (Some(body), Some(host)) = (body, self.host.upgrade()) {
            host.emit(&Event::Message { body });
        }
        Ok(())
    }
}

/// The WebBrowser control's events (DWebBrowserEvents2).
#[implement(IDispatch)]
struct Sink {
    host: std::rc::Weak<Host>,
}

impl IDispatch_Impl for Sink_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }
    fn GetTypeInfo(&self, _: u32, _: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(
        &self,
        _: *const GUID,
        _: *const PCWSTR,
        _: u32,
        _: u32,
        _: *mut i32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(
        &self,
        id: i32,
        _: *const GUID,
        _: u32,
        _: DISPATCH_FLAGS,
        params: *const DISPPARAMS,
        _: *mut VARIANT,
        _: *mut EXCEPINFO,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        let Some(host) = self.host.upgrade() else {
            return Ok(());
        };
        let a = args(params);
        let subject = || a.first().and_then(|v| dispatch_of(v));
        match id {
            DISPID_BEFORENAVIGATE2 if host.is_top(subject().as_ref()) => {
                host.emit(&Event::LoadStart)
            }
            DISPID_NAVIGATECOMPLETE2 if host.is_top(subject().as_ref()) => {
                host.url();
                host.inject();
            }
            DISPID_DOCUMENTCOMPLETE if host.is_top(subject().as_ref()) => {
                host.url();
                if let Some(browser) = host.browser() {
                    host.apply_zoom(&browser);
                }
                host.inject();
                host.emit(&Event::LoadEnd);
            }
            DISPID_NAVIGATEERROR if host.is_top(subject().as_ref()) => host.emit(&Event::LoadEnd),
            DISPID_COMMANDSTATECHANGE => {
                if let (Some(command), Some(enabled)) = (
                    a.first().and_then(|v| i32_of(v)),
                    a.get(1).and_then(|v| bool_of(v)),
                ) {
                    host.history(command, enabled);
                }
            }
            // (ppDisp, Cancel, dwFlags, bstrUrlContext, bstrUrl): a tab, not a window.
            DISPID_NEWWINDOW3 => {
                if let Some(cancel) = a.get(1) {
                    set_cancel(cancel);
                }
                if let Some(url) = a.get(4).and_then(|v| string_of(v)) {
                    host.emit(&Event::NewWindow { url });
                }
            }
            // (IsChildWindow, Cancel): a page's window.close() can't close the pane.
            DISPID_WINDOWCLOSING => {
                if let Some(cancel) = a.get(1) {
                    set_cancel(cancel);
                }
            }
            // (ActiveDocument, Cancel): downloads would open IE's own dialogs.
            DISPID_FILEDOWNLOAD => {
                let document = a.first().and_then(|v| bool_of(v)).unwrap_or(true);
                if !document {
                    if let Some(cancel) = a.get(1) {
                        set_cancel(cancel);
                    }
                    host.emit(&Event::Notice {
                        text: "Trident doesn't download files; open the page in another engine"
                            .into(),
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

// --- the process -----------------------------------------------------------------

/// If this process was started as a Trident helper, run it and return true.
pub(crate) fn run_if_helper() -> bool {
    if std::env::args().nth(1).as_deref() != Some(super::HOST_FLAG) {
        return false;
    }
    if let Err(e) = unsafe { run() } {
        eprintln!("trident host: {e:#}");
        std::process::exit(1);
    }
    true
}

unsafe fn run() -> anyhow::Result<()> {
    OleInitialize(None)?;
    let instance = GetModuleHandleW(None)?;
    let class = w!("BrowserTridentHost");
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        w!("Trident"),
        WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
        0,
        0,
        640,
        480,
        None,
        None,
        Some(instance.into()),
        None,
    )?;

    let commands = Arc::new(Mutex::new(VecDeque::new()));
    let host = Rc::new(Host {
        hwnd,
        browser: RefCell::new(None),
        init: RefCell::new(String::new()),
        back: Cell::new(false),
        forward: Cell::new(false),
        zoom: Cell::new(100),
        commands: commands.clone(),
    });
    HOST.with(|h| *h.borrow_mut() = Some(host.clone()));

    let frame: IOleInPlaceFrame = Frame { hwnd }.into();
    let external: IDispatch = External {
        host: Rc::downgrade(&host),
    }
    .into();
    let site: IOleClientSite = Site {
        host: host.clone(),
        frame,
        external,
        security: Security.into(),
    }
    .into();
    let object: IOleObject = CoCreateInstance(&WebBrowser, None, CLSCTX_INPROC_SERVER)?;
    object.SetClientSite(&site)?;
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    object.DoVerb(
        OLEIVERB_INPLACEACTIVATE.0,
        std::ptr::null(),
        &site,
        0,
        hwnd,
        &rect,
    )?;
    let browser: IWebBrowser2 = object.cast()?;
    // Script errors and similar would otherwise pop up IE dialogs.
    browser.SetSilent(VARIANT_BOOL::from(true))?;
    *host.browser.borrow_mut() = Some(browser.clone());

    let sink: IDispatch = Sink {
        host: Rc::downgrade(&host),
    }
    .into();
    let points: IConnectionPointContainer = browser.cast()?;
    let point = points.FindConnectionPoint(&DWebBrowserEvents2::IID)?;
    point.Advise(&sink)?;

    // Commands arrive on stdin; the window procedure runs them on this thread.
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if let Some(command) = protocol::decode::<Command>(&line) {
                if let Ok(mut q) = commands.lock() {
                    q.push_back(command);
                }
                let _ = PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    WM_COMMANDS,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        // The browser is gone: so is this.
        if let Ok(mut q) = commands.lock() {
            q.push_back(Command::Quit);
        }
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_COMMANDS,
            WPARAM(0),
            LPARAM(0),
        );
    });

    host.emit(&Event::Ready {
        hwnd: hwnd.0 as i64,
    });

    let active: Option<IOleInPlaceActiveObject> = browser.cast().ok();
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        // Give the control first look at keys: Tab between fields, Ctrl+C, …
        if (WM_KEYFIRST..=WM_KEYLAST).contains(&msg.message) {
            if let Some(active) = &active {
                let handled =
                    (Interface::vtable(active).TranslateAccelerator)(active.as_raw(), &msg);
                if handled == S_OK {
                    continue;
                }
            }
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = point;
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            let host = HOST.with(|h| h.borrow().clone());
            if let Some(browser) = host.and_then(|h| h.browser()) {
                if let Ok(object) = browser.cast::<IOleInPlaceObject>() {
                    let mut rect = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rect);
                    let _ = object.SetObjectRects(&rect, &rect);
                }
            }
            LRESULT(0)
        }
        WM_COMMANDS => {
            if let Some(host) = HOST.with(|h| h.borrow().clone()) {
                loop {
                    let next = host.commands.lock().ok().and_then(|mut q| q.pop_front());
                    let Some(command) = next else { break };
                    host.run(command);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_activex_java_and_downloads_are_refused() {
        assert!(forbidden_action(URLACTION_ACTIVEX_OVERRIDE_OBJECT_SAFETY));
        assert!(forbidden_action(URLACTION_JAVA_MIN));
        assert!(forbidden_action(URLACTION_SHELL_FILE_DOWNLOAD));
        assert!(forbidden_action(URLACTION_DOWNLOAD_UNSIGNED_ACTIVEX));
        assert!(
            !forbidden_action(0x1200),
            "safe controls still run (URLACTION_ACTIVEX_RUN)"
        );
        assert!(
            !forbidden_action(0x1400),
            "running page scripts stays IE's call"
        );
    }
}
