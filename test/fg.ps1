# Test helper: print the foreground window's class and title (run in the desktop session).
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Text;
public static class Fg {
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  public static string Now() { var h = GetForegroundWindow(); var c = new StringBuilder(256); var t = new StringBuilder(256); GetClassNameW(h, c, 256); GetWindowTextW(h, t, 256); return "foreground: class=" + c + " title=" + t; }
}
'@
[Fg]::Now()
