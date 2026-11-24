//! 接收从资源管理器拖入窗口的文件。
//!
//! 为什么不用 Slint 的 `DropArea`：Slint 1.19 的 winit 后端没有对接操作系统级的文件拖放
//! （`i-slint-backend-winit` 里没有任何 `DroppedFile` / `HoveredFile` 分支），`DropArea` 只能
//! 收到应用内部 `DragArea` 发起的拖拽。
//!
//! 更关键的是：winit 在 Windows 上默认 `drag_and_drop = true`，窗口一创建就 `RegisterDragDrop`
//! 了它自己的 `IDropTarget`。资源管理器拖文件过来时走的是 OLE 通道，事件全部进了 winit 的手，
//! 再被 Slint 后端丢掉——所以光挂 `DragAcceptFiles` 收 `WM_DROPFILES` 是没用的（那个消息
//! 根本不会发出来，只有自检里手动 PostMessage 才会）。
//!
//! 这里做两件事：
//! 1. `RevokeDragDrop` 掉 winit 的目标，换成我们自己的 `IDropTarget`，走 OLE 通道收文件；
//! 2. 同时保留 `DragAcceptFiles` + `WM_DROPFILES` 子类化兜底，并给消息放行 UIPI。

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    const WM_DROPFILES: u32 = 0x0233;
    const WM_COPYDATA: u32 = 0x004A;
    /// WM_COPYGLOBALDATA：shell 传 OLE 数据时用的，UIPI 也拦它
    const WM_COPYGLOBALDATA: u32 = 0x0049;

    /// 子类化 ID，随便取一个不会撞的值
    const SUBCLASS_ID: usize = 0x4852_5100;
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_ACCEPTFILES: isize = 0x0010;
    /// ChangeWindowMessageFilterEx 的 MSGFLT_ALLOW
    const MSGFLT_ALLOW: u32 = 1;

    /// 子类过程里不能安全地碰界面状态（那边是 winit 的消息循环），
    /// 先攒起来，由界面定时器取走。
    static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    /// 有没有文件正悬停在窗口上（拿来画拖入提示）
    static HOVERING: AtomicBool = AtomicBool::new(false);
    /// 成功落下的次数，自检/日志用
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
    static SUBCLASSED: AtomicBool = AtomicBool::new(false);

    // ───────────────────────── Win32 声明 ─────────────────────────

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn DragAcceptFiles(hwnd: isize, accept: i32);
        fn DragQueryFileW(hdrop: isize, ifile: u32, file: *mut u16, cch: u32) -> u32;
        fn DragFinish(hdrop: isize);
    }

    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(
            hwnd: isize,
            proc_: Option<
                unsafe extern "system" fn(isize, u32, usize, isize, usize, usize) -> isize,
            >,
            id: usize,
            data: usize,
        ) -> i32;
        fn DefSubclassProc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn PostMessageW(hwnd: isize, msg: u32, wp: usize, lp: isize) -> i32;
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        fn ChangeWindowMessageFilterEx(
            hwnd: isize,
            msg: u32,
            action: u32,
            changed: *mut c_void,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn RegisterDragDrop(hwnd: isize, target: *mut c_void) -> i32;
        fn RevokeDragDrop(hwnd: isize) -> i32;
        fn OleInitialize(reserved: *mut c_void) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
        fn GetTokenInformation(
            token: isize,
            class: u32,
            info: *mut c_void,
            len: u32,
            ret: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn CloseHandle(h: isize) -> i32;
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(h: isize) -> *mut u8;
        fn GlobalUnlock(h: isize) -> i32;
    }

    // ───────────────────────── 状态上报 ─────────────────────────

    #[derive(Default, Clone, Copy)]
    pub struct Report {
        pub hwnd: isize,
        /// DragAcceptFiles 生效（WS_EX_ACCEPTFILES）
        pub drag_accept: bool,
        /// WM_DROPFILES 子类化成功
        pub subclass: bool,
        /// 自己的 OLE IDropTarget 注册成功
        pub ole: bool,
        /// 是否接管了 winit 原本占着的 OLE 拖放目标
        pub revoked: bool,
        /// 进程是否管理员权限（管理员从资源管理器拖不进来，是系统限制）
        pub elevated: bool,
    }

    impl Report {
        /// 两条通道至少通了一条
        pub fn ok(&self) -> bool {
            self.ole || self.subclass
        }

        pub fn describe(&self) -> String {
            let ch = |b: bool| if b { "就绪" } else { "不可用" };
            let mut s = format!(
                "HWND=0x{:X} · OLE {} · WM_DROPFILES {} · 进程权限 {}",
                self.hwnd,
                ch(self.ole),
                ch(self.subclass),
                if self.elevated { "管理员" } else { "普通" }
            );
            if self.revoked {
                s.push_str(" · 已接管 winit 的拖放目标");
            }
            if self.elevated {
                s.push_str(" ⚠ 管理员权限下 Windows 不允许从资源管理器拖放，请用普通权限启动");
            }
            s
        }
    }

    /// 当前进程是否提升（管理员）。提升后 shell 的拖放会被 UIPI 直接拦住。
    pub fn is_elevated() -> bool {
        const TOKEN_QUERY: u32 = 0x0008;
        const TOKEN_ELEVATION: u32 = 20;
        unsafe {
            let mut token: isize = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevated: u32 = 0;
            let mut ret: u32 = 0;
            let ok = GetTokenInformation(
                token,
                TOKEN_ELEVATION,
                &mut elevated as *mut u32 as *mut c_void,
                4,
                &mut ret,
            );
            CloseHandle(token);
            ok != 0 && elevated != 0
        }
    }

    // ───────────────────────── HDROP 读取 ─────────────────────────

    /// 从 HDROP 句柄里读出所有路径（不负责 DragFinish）。
    unsafe fn read_hdrop(hdrop: isize) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        // 0xFFFFFFFF 表示查询文件数量
        let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0) };
        for i in 0..count {
            let len = unsafe { DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0) };
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = unsafe { DragQueryFileW(hdrop, i, buf.as_mut_ptr(), len + 1) };
            if n > 0 {
                let wide: Vec<u16> = buf[..n as usize].to_vec();
                paths.push(PathBuf::from(OsString::from_wide(&wide)));
            }
        }
        paths
    }

    // ───────────────────────── 兜底：WM_DROPFILES ─────────────────────────

    unsafe extern "system" fn subclass_proc(
        hwnd: isize,
        msg: u32,
        wp: usize,
        lp: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        if msg == WM_DROPFILES {
            let hdrop = wp as isize;
            let paths = unsafe { read_hdrop(hdrop) };
            unsafe { DragFinish(hdrop) };
            if !paths.is_empty() {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut q) = PENDING.lock() {
                    q.extend(paths);
                }
            }
            return 0;
        }
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    // ───────────────────────── 主通道：OLE IDropTarget ─────────────────────────

    #[repr(C)]
    #[derive(PartialEq, Eq)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    const IID_IUNKNOWN: Guid = Guid {
        d1: 0,
        d2: 0,
        d3: 0,
        d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
    };
    const IID_IDROPTARGET: Guid = Guid {
        d1: 0x0000_0122,
        d2: 0,
        d3: 0,
        d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46],
    };

    const S_OK: i32 = 0;
    const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
    const DROPEFFECT_NONE: u32 = 0;
    const DROPEFFECT_COPY: u32 = 1;

    const CF_HDROP: u16 = 15;
    const DVASPECT_CONTENT: u32 = 1;
    const TYMED_HGLOBAL: u32 = 1;

    #[repr(C)]
    struct PointL {
        x: i32,
        y: i32,
    }

    /// FORMATETC（x64 下 32 字节：cfFormat 后要补 6 字节才对齐指针）
    #[repr(C)]
    struct FormatEtc {
        cf_format: u16,
        ptd: *mut c_void,
        dw_aspect: u32,
        lindex: i32,
        tymed: u32,
    }

    /// STGMEDIUM（24 字节：tymed 后补 4 字节）
    #[repr(C)]
    struct StgMedium {
        tymed: u32,
        h_global: isize,
        punk_for_release: isize,
    }

    #[repr(C)]
    struct DropTargetVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        drag_enter:
            unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const PointL, *mut u32) -> i32,
        drag_over: unsafe extern "system" fn(*mut c_void, u32, *const PointL, *mut u32) -> i32,
        drag_leave: unsafe extern "system" fn(*mut c_void) -> i32,
        drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const PointL, *mut u32) -> i32,
    }

    #[repr(C)]
    struct DropTarget {
        vtbl: *const DropTargetVtbl,
    }

    // IUnknown 部分：本对象活到进程结束，引用计数只是个形式
    unsafe extern "system" fn query_interface(
        this: *mut c_void,
        riid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        if out.is_null() || riid.is_null() {
            return E_NOINTERFACE;
        }
        let want = unsafe { &*riid };
        if *want == IID_IUNKNOWN || *want == IID_IDROPTARGET {
            unsafe { *out = this };
            S_OK
        } else {
            unsafe { *out = std::ptr::null_mut() };
            E_NOINTERFACE
        }
    }
    unsafe extern "system" fn add_ref(_this: *mut c_void) -> u32 {
        2
    }
    unsafe extern "system" fn release(_this: *mut c_void) -> u32 {
        1
    }

    /// 从 IDataObject 里取出 CF_HDROP 路径。内部已 DragFinish 释放 medium。
    unsafe fn data_object_paths(data_obj: *mut c_void) -> Vec<PathBuf> {
        if data_obj.is_null() {
            return Vec::new();
        }
        let vtbl = unsafe { *(data_obj as *const *const usize) };
        if vtbl.is_null() {
            return Vec::new();
        }
        // IDataObject vtable: QI / AddRef / Release / GetData ← 第 4 个槽
        let get_data: unsafe extern "system" fn(
            *mut c_void,
            *const FormatEtc,
            *mut StgMedium,
        ) -> i32 = unsafe { std::mem::transmute(*vtbl.add(3)) };

        let fmt = FormatEtc {
            cf_format: CF_HDROP,
            ptd: std::ptr::null_mut(),
            dw_aspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        };
        let mut med: StgMedium = unsafe { std::mem::zeroed() };
        if unsafe { get_data(data_obj, &fmt, &mut med) } < 0 {
            return Vec::new();
        }
        if med.h_global == 0 {
            return Vec::new();
        }
        let paths = unsafe { read_hdrop(med.h_global) };
        unsafe { DragFinish(med.h_global) };
        paths
    }

    unsafe extern "system" fn drag_enter(
        _this: *mut c_void,
        data: *mut c_void,
        _keys: u32,
        _pt: *const PointL,
        effect: *mut u32,
    ) -> i32 {
        let has = !unsafe { data_object_paths(data) }.is_empty();
        HOVERING.store(has, Ordering::SeqCst);
        if !effect.is_null() {
            unsafe { *effect = if has { DROPEFFECT_COPY } else { DROPEFFECT_NONE } };
        }
        S_OK
    }

    unsafe extern "system" fn drag_over(
        _this: *mut c_void,
        _keys: u32,
        _pt: *const PointL,
        effect: *mut u32,
    ) -> i32 {
        if !effect.is_null() {
            unsafe {
                *effect = if HOVERING.load(Ordering::SeqCst) {
                    DROPEFFECT_COPY
                } else {
                    DROPEFFECT_NONE
                }
            };
        }
        S_OK
    }

    unsafe extern "system" fn drag_leave(_this: *mut c_void) -> i32 {
        HOVERING.store(false, Ordering::SeqCst);
        S_OK
    }

    unsafe extern "system" fn drop(
        _this: *mut c_void,
        data: *mut c_void,
        _keys: u32,
        _pt: *const PointL,
        effect: *mut u32,
    ) -> i32 {
        HOVERING.store(false, Ordering::SeqCst);
        let paths = unsafe { data_object_paths(data) };
        if paths.is_empty() {
            if !effect.is_null() {
                unsafe { *effect = DROPEFFECT_NONE };
            }
        } else {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut q) = PENDING.lock() {
                q.extend(paths);
            }
            if !effect.is_null() {
                unsafe { *effect = DROPEFFECT_COPY };
            }
        }
        S_OK
    }

    static DROP_TARGET_VTBL: DropTargetVtbl = DropTargetVtbl {
        query_interface,
        add_ref,
        release,
        drag_enter,
        drag_over,
        drag_leave,
        drop,
    };

    /// 把窗口上的拖放目标换成自己的。
    /// winit 默认会注册它自己的（`drag_and_drop = true`），先撤销再注册，否则资源管理器
    /// 拖过来的文件全进了 winit 的手、被 Slint 丢掉。
    /// 返回 (是否注册成功, 是否撤销掉了别人占着的目标)
    fn register_ole(hwnd: isize) -> (bool, bool) {
        static TARGET: OnceLock<usize> = OnceLock::new();
        let target = *TARGET.get_or_init(|| {
            let boxed = Box::new(DropTarget {
                vtbl: &DROP_TARGET_VTBL,
            });
            // 故意泄漏：生命周期跟窗口一样，不做引用计数回收
            Box::into_raw(boxed) as usize
        });
        unsafe {
            let revoked = RevokeDragDrop(hwnd) == S_OK;
            if RegisterDragDrop(hwnd, target as *mut c_void) == S_OK {
                return (true, revoked);
            }
            // winit 没开拖放时不会 OleInitialize，自己补一次再注册
            OleInitialize(std::ptr::null_mut());
            (RegisterDragDrop(hwnd, target as *mut c_void) == S_OK, revoked)
        }
    }

    // ───────────────────────── 对外接口 ─────────────────────────

    /// 登记窗口接收文件拖放。`hwnd` 取自 Slint 的 `window_handle()`。
    pub fn install(hwnd: isize) -> Report {
        let mut rep = Report {
            hwnd,
            elevated: is_elevated(),
            ..Default::default()
        };
        if hwnd == 0 {
            return rep;
        }
        unsafe {
            // 提升进程要收高权限窗口的消息，得先放行这几个（OLE 传数据靠 WM_COPYGLOBALDATA）
            ChangeWindowMessageFilterEx(hwnd, WM_DROPFILES, MSGFLT_ALLOW, std::ptr::null_mut());
            ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, std::ptr::null_mut());
            ChangeWindowMessageFilterEx(hwnd, WM_COPYGLOBALDATA, MSGFLT_ALLOW, std::ptr::null_mut());

            DragAcceptFiles(hwnd, 1);
            rep.drag_accept = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_ACCEPTFILES != 0;

            rep.subclass = if SUBCLASSED.load(Ordering::SeqCst) {
                true
            } else {
                let ok = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) != 0;
                if ok {
                    SUBCLASSED.store(true, Ordering::SeqCst);
                }
                ok
            };
        }
        let (ole, revoked) = register_ole(hwnd);
        rep.ole = ole;
        rep.revoked = revoked;
        rep
    }

    /// 取走自上次调用以来拖入的路径。
    pub fn take_dropped() -> Vec<PathBuf> {
        PENDING
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    /// 是否有文件正悬停在窗口上（画拖入提示用）。
    pub fn is_hovering() -> bool {
        HOVERING.load(Ordering::SeqCst)
    }

    /// 累计成功接收的次数。
    pub fn drop_count() -> usize {
        DROP_COUNT.load(Ordering::SeqCst)
    }

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(hwnd: isize, attr: u32, value: *const i32, size: u32) -> i32;
    }

    /// 让系统绘制的标题栏跟着主题走（暗色界面配白标题栏太割裂）。
    pub fn set_dark_titlebar(hwnd: isize, dark: bool) {
        if hwnd == 0 {
            return;
        }
        let flag: i32 = if dark { 1 } else { 0 };
        unsafe {
            // 20 = DWMWA_USE_IMMERSIVE_DARK_MODE（Win10 20H1+），19 是老版本号
            if DwmSetWindowAttribute(hwnd, 20, &flag, 4) != 0 {
                DwmSetWindowAttribute(hwnd, 19, &flag, 4);
            }
        }
    }

    /// 自检用：把路径打包成 HDROP 投给窗口，等价于从资源管理器拖进来一次。
    /// 只能在进程内调用——GMEM 句柄是进程私有的，别的进程伪造不了 WM_DROPFILES。
    pub fn post_drop(hwnd: isize, paths: &[PathBuf]) -> bool {
        if hwnd == 0 || paths.is_empty() {
            return false;
        }
        // DROPFILES 头 20 字节：pFiles(4) + pt(8) + fNC(4) + fWide(4)
        let mut list: Vec<u16> = Vec::new();
        for p in paths {
            let s = p.to_string_lossy();
            list.extend(s.encode_utf16());
            list.push(0);
        }
        list.push(0);
        let bytes = list.len() * 2;
        let total = 20 + bytes;
        unsafe {
            // GMEM_MOVEABLE | GMEM_ZEROINIT
            let h = GlobalAlloc(0x0042, total);
            if h == 0 {
                return false;
            }
            let base = GlobalLock(h);
            if base.is_null() {
                return false;
            }
            *(base as *mut u32) = 20; // pFiles
            *(base.add(16) as *mut u32) = 1; // fWide = TRUE
            std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, base.add(20), bytes);
            GlobalUnlock(h);
            PostMessageW(hwnd, WM_DROPFILES, h as usize, 0) != 0
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use std::path::PathBuf;

    #[derive(Default, Clone, Copy)]
    pub struct Report {
        pub hwnd: isize,
        pub drag_accept: bool,
        pub subclass: bool,
        pub ole: bool,
        pub revoked: bool,
        pub elevated: bool,
    }

    impl Report {
        pub fn ok(&self) -> bool {
            false
        }
        pub fn describe(&self) -> String {
            "当前平台不支持".into()
        }
    }

    pub fn install(_hwnd: isize) -> Report {
        Report::default()
    }

    pub fn take_dropped() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn is_hovering() -> bool {
        false
    }

    pub fn drop_count() -> usize {
        0
    }

    pub fn post_drop(_hwnd: isize, _paths: &[PathBuf]) -> bool {
        false
    }

    pub fn set_dark_titlebar(_hwnd: isize, _dark: bool) {}
}

pub use imp::{
    Report, drop_count, install, is_hovering, post_drop, set_dark_titlebar, take_dropped,
};

/// 从 Slint 窗口取出原生窗口句柄（Windows 的 HWND）。
#[cfg(target_os = "windows")]
pub fn hwnd_of(window: &slint::Window) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().window_handle() {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::Win32(w) => w.hwnd.get(),
            _ => 0,
        },
        Err(_) => 0,
    }
}
