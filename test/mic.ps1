# Test helper: inspect and drive microphone mute state independently of mutekey.
#   mic.ps1 status        list active capture endpoints and their mute state
#   mic.ps1 mute|unmute   set every active capture endpoint (like a third-party app would)
#   mic.ps1 mute-one N    mute only endpoint N (0-based)
#   mic.ps1 hotkey        press Win+Shift+A (must run in the desktop session)
#   mic.ps1 hotkey-free   is Win+Shift+A free to register? (False while mutekey runs)
#   mic.ps1 badge         is the mutekey badge window visible, and where?
#   mic.ps1 click|rclick -X x -Y y   left/right-click at screen coordinates (desktop session)
#   mic.ps1 scroll -X x -Y y -Index n  scroll n notches down (negative = up) at x,y
param([Parameter(Mandatory)][string]$Action, [int]$Index = 0, [int]$X = 0, [int]$Y = 0)

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

[ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioEndpointVolume {
    int RegisterControlChangeNotify(IntPtr p); int UnregisterControlChangeNotify(IntPtr p);
    int GetChannelCount(out uint c);
    int SetMasterVolumeLevel(float f, ref Guid g); int SetMasterVolumeLevelScalar(float f, ref Guid g);
    int GetMasterVolumeLevel(out float f); int GetMasterVolumeLevelScalar(out float f);
    int SetChannelVolumeLevel(uint n, float f, ref Guid g); int SetChannelVolumeLevelScalar(uint n, float f, ref Guid g);
    int GetChannelVolumeLevel(uint n, out float f); int GetChannelVolumeLevelScalar(uint n, out float f);
    int SetMute([MarshalAs(UnmanagedType.Bool)] bool m, ref Guid g);
    int GetMute([MarshalAs(UnmanagedType.Bool)] out bool m);
}
[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
    int Activate(ref Guid id, int ctx, IntPtr p, [MarshalAs(UnmanagedType.IUnknown)] out object o);
    int OpenPropertyStore(int access, out IntPtr store);
    int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
    int GetState(out int s);
}
[ComImport, Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceCollection { int GetCount(out uint c); int Item(uint i, out IMMDevice d); }
[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator { int EnumAudioEndpoints(int flow, int mask, out IMMDeviceCollection c); }
[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumerator {}

public static class Mic {
    static List<IAudioEndpointVolume> All(out List<string> ids) {
        var e = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        IMMDeviceCollection c; Marshal.ThrowExceptionForHR(e.EnumAudioEndpoints(1 /*eCapture*/, 1 /*ACTIVE*/, out c));
        uint n; c.GetCount(out n);
        var list = new List<IAudioEndpointVolume>(); ids = new List<string>();
        var iid = typeof(IAudioEndpointVolume).GUID;
        for (uint i = 0; i < n; i++) {
            IMMDevice d; c.Item(i, out d);
            string id; d.GetId(out id); ids.Add(id);
            object o; Marshal.ThrowExceptionForHR(d.Activate(ref iid, 23 /*CLSCTX_ALL*/, IntPtr.Zero, out o));
            list.Add((IAudioEndpointVolume)o);
        }
        return list;
    }
    public static string Status() {
        List<string> ids; var mics = All(out ids);
        if (mics.Count == 0) return "no capture endpoints";
        var lines = new List<string>();
        for (int i = 0; i < mics.Count; i++) { bool m; mics[i].GetMute(out m); lines.Add(i + " muted=" + m + " " + ids[i]); }
        return string.Join("\n", lines);
    }
    public static void SetAll(bool mute) {
        List<string> ids; var g = Guid.Empty;
        foreach (var m in All(out ids)) m.SetMute(mute, ref g);
    }
    public static void SetOne(int index, bool mute) {
        List<string> ids; var g = Guid.Empty;
        All(out ids)[index].SetMute(mute, ref g);
    }

    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    public static void PressWinShiftA() {
        const uint UP = 2;
        keybd_event(0x5B, 0, 0, UIntPtr.Zero); keybd_event(0x10, 0, 0, UIntPtr.Zero); keybd_event(0x41, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(50);
        keybd_event(0x41, 0, UP, UIntPtr.Zero); keybd_event(0x10, 0, UP, UIntPtr.Zero); keybd_event(0x5B, 0, UP, UIntPtr.Zero);
    }

    [DllImport("user32.dll")] static extern bool RegisterHotKey(IntPtr h, int id, uint mods, uint vk);
    [DllImport("user32.dll")] static extern bool UnregisterHotKey(IntPtr h, int id);
    public static bool HotkeyFree() {
        bool ok = RegisterHotKey(IntPtr.Zero, 0x7123, 0x8 /*WIN*/ | 0x4 /*SHIFT*/, 0x41);
        if (ok) UnregisterHotKey(IntPtr.Zero, 0x7123);
        return ok;
    }

    [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
    public static void Click(int x, int y, bool right) {
        SetCursorPos(x, y); System.Threading.Thread.Sleep(100);
        mouse_event(right ? 0x8u : 0x2u, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
        mouse_event(right ? 0x10u : 0x4u, 0, 0, 0, UIntPtr.Zero);
    }

    public static void Scroll(int x, int y, int notches) {
        SetCursorPos(x, y); System.Threading.Thread.Sleep(100);
        mouse_event(0x800 /*WHEEL*/, 0, 0, unchecked((uint)(-120 * notches)), UIntPtr.Zero);
    }

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindowW(string cls, string title);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    public static string Badge() {
        var h = FindWindowW("mutekey", "mutekey badge");
        if (h == IntPtr.Zero) return "badge window not found (mutekey not running?)";
        RECT r; GetWindowRect(h, out r);
        return "badge visible=" + IsWindowVisible(h) + " rect=" + r.L + "," + r.T + " " + (r.R - r.L) + "x" + (r.B - r.T);
    }
}
'@

switch ($Action) {
    'status'      { [Mic]::Status() }
    'mute'        { [Mic]::SetAll($true); [Mic]::Status() }
    'unmute'      { [Mic]::SetAll($false); [Mic]::Status() }
    'mute-one'    { [Mic]::SetOne($Index, $true); [Mic]::Status() }
    'hotkey'      { [Mic]::PressWinShiftA(); 'pressed Win+Shift+A' }
    'hotkey-free' { "hotkey free=" + [Mic]::HotkeyFree() }
    'badge'       { [Mic]::Badge() }
    'click'       { [Mic]::Click($X, $Y, $false); "clicked $X,$Y" }
    'scroll'      { [Mic]::Scroll($X, $Y, $Index); "scrolled $Index at $X,$Y" }
    'rclick'      { [Mic]::Click($X, $Y, $true); "right-clicked $X,$Y" }
    default       { throw "unknown action $Action" }
}
