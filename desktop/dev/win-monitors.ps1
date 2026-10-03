# Ground truth from Windows itself: every monitor's physical rectangle, DPI scale and primary flag, read from a
# Per-Monitor-V2 DPI-aware process (the same mode the Glide engine runs in).
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class Mon {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] public struct MI { public int cb; public RECT rc, work; public uint flags; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dev; }
  delegate bool Proc(IntPtr h, IntPtr dc, ref RECT r, IntPtr d);
  [DllImport("user32.dll")] static extern bool EnumDisplayMonitors(IntPtr dc, IntPtr clip, Proc p, IntPtr d);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool GetMonitorInfo(IntPtr h, ref MI mi);
  [DllImport("shcore.dll")] static extern int GetDpiForMonitor(IntPtr h, int type, out uint x, out uint y);
  [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  public static string[] All() {
    SetProcessDpiAwarenessContext(new IntPtr(-4));   // PER_MONITOR_AWARE_V2
    var rows = new List<string>();
    EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (IntPtr h, IntPtr dc, ref RECT r, IntPtr d) => {
      var mi = new MI(); mi.cb = Marshal.SizeOf(typeof(MI)); GetMonitorInfo(h, ref mi);
      uint dx, dy; GetDpiForMonitor(h, 0, out dx, out dy);              // MDT_EFFECTIVE_DPI
      double scale = dx / 96.0;
      int w = mi.rc.R - mi.rc.L, hh = mi.rc.B - mi.rc.T;
      rows.Add(string.Format("{0}: physical rect ({1},{2}) to ({3},{4}) = {5}x{6} px | dpi {7} => scale {8:0.##} | logical size {9:0}x{10:0} | {11}",
        mi.dev, mi.rc.L, mi.rc.T, mi.rc.R, mi.rc.B, w, hh, dx, scale, w / scale, hh / scale, (mi.flags & 1) != 0 ? "PRIMARY" : "secondary"));
      return true; }, IntPtr.Zero);
    return rows.ToArray();
  }
}
'@
[Mon]::All()
