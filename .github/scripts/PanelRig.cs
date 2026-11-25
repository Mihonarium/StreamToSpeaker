// What Test-PanelClick.ps1 needs from Windows that PowerShell has no
// command for: real mouse input (SendInput), pictures of the whole desktop
// with the mouse pointer drawn in (GDI, saved as PNG), the windows of a
// process and the one that has the foreground, and the rectangle the
// taskbar gives a tray icon. Written in C# 5, which Windows PowerShell's
// Add-Type compiles.

using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.IO.Compression;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace PanelRig
{
    public static class Native
    {
        [StructLayout(LayoutKind.Sequential)]
        public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }

        [StructLayout(LayoutKind.Sequential)]
        public struct POINT { public int X; public int Y; }

        [StructLayout(LayoutKind.Sequential)]
        public struct MOUSEINPUT
        {
            public int dx;
            public int dy;
            public uint mouseData;
            public uint dwFlags;
            public uint time;
            public IntPtr dwExtraInfo;
        }

        // INPUT with its union reduced to the mouse member, the largest
        // one, so the size matches what SendInput expects.
        [StructLayout(LayoutKind.Sequential)]
        public struct INPUT
        {
            public uint type;
            public MOUSEINPUT mi;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct NOTIFYICONIDENTIFIER
        {
            public uint cbSize;
            public IntPtr hWnd;
            public uint uID;
            public Guid guidItem;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct CURSORINFO
        {
            public uint cbSize;
            public uint flags;
            public IntPtr hCursor;
            public POINT ptScreenPos;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct ICONINFO
        {
            public bool fIcon;
            public uint xHotspot;
            public uint yHotspot;
            public IntPtr hbmMask;
            public IntPtr hbmColor;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct BITMAPINFOHEADER
        {
            public uint biSize;
            public int biWidth;
            public int biHeight;
            public ushort biPlanes;
            public ushort biBitCount;
            public uint biCompression;
            public uint biSizeImage;
            public int biXPelsPerMeter;
            public int biYPelsPerMeter;
            public uint biClrUsed;
            public uint biClrImportant;
        }

        public delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);

        [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc f, IntPtr l);
        [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
        [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
        [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
        [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
        [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int index);
        [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
        [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
        [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
        [DllImport("user32.dll", SetLastError = true)] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
        [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
        [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
        [DllImport("user32.dll")] public static extern uint GetDoubleClickTime();
        [DllImport("user32.dll", SetLastError = true)] public static extern bool SetProcessDpiAwarenessContext(IntPtr context);
        [DllImport("user32.dll")] public static extern IntPtr GetDC(IntPtr h);
        [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr h, IntPtr dc);
        [DllImport("user32.dll")] public static extern bool GetCursorInfo(ref CURSORINFO info);
        [DllImport("user32.dll")] public static extern bool GetIconInfo(IntPtr icon, out ICONINFO info);
        [DllImport("user32.dll")] public static extern bool DrawIconEx(IntPtr dc, int x, int y, IntPtr icon, int w, int h, uint step, IntPtr brush, uint flags);
        [DllImport("gdi32.dll")] public static extern IntPtr CreateCompatibleDC(IntPtr dc);
        [DllImport("gdi32.dll")] public static extern bool DeleteDC(IntPtr dc);
        [DllImport("gdi32.dll")] public static extern IntPtr CreateDIBSection(IntPtr dc, ref BITMAPINFOHEADER header, uint usage, out IntPtr bits, IntPtr section, uint offset);
        [DllImport("gdi32.dll")] public static extern IntPtr SelectObject(IntPtr dc, IntPtr obj);
        [DllImport("gdi32.dll")] public static extern bool DeleteObject(IntPtr obj);
        [DllImport("gdi32.dll", SetLastError = true)] public static extern bool BitBlt(IntPtr dst, int x, int y, int w, int h, IntPtr src, int sx, int sy, uint rop);
        [DllImport("shell32.dll")] public static extern int Shell_NotifyIconGetRect(ref NOTIFYICONIDENTIFIER id, out RECT r);
    }

    /// <summary>A rectangle in physical pixels, as Windows gives them.</summary>
    public sealed class Box
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;

        public Box(int left, int top, int right, int bottom) { Left = left; Top = top; Right = right; Bottom = bottom; }

        public int Width { get { return Right - Left; } }
        public int Height { get { return Bottom - Top; } }
        public int CenterX { get { return (Left + Right) / 2; } }
        public int CenterY { get { return (Top + Bottom) / 2; } }
        public bool IsEmpty { get { return Width <= 0 || Height <= 0; } }

        public bool Contains(int x, int y) { return x >= Left && x < Right && y >= Top && y < Bottom; }

        public override string ToString()
        {
            return string.Format("{0}x{1} at {2},{3}", Width, Height, Left, Top);
        }
    }

    /// <summary>One top-level window.</summary>
    public sealed class Win
    {
        public IntPtr Handle;
        public uint Pid;
        public string Class;
        public string Title;
        public bool Visible;
        public Box Rect;

        public override string ToString()
        {
            return string.Format("{0} \"{1}\" {2} {3}", Class, Title, Visible ? "shown" : "hidden", Rect);
        }
    }

    public static class Windows
    {
        public static Win Describe(IntPtr h)
        {
            var w = new Win();
            w.Handle = h;
            uint pid;
            Native.GetWindowThreadProcessId(h, out pid);
            w.Pid = pid;
            var s = new StringBuilder(256);
            Native.GetClassNameW(h, s, s.Capacity);
            w.Class = s.ToString();
            s.Length = 0;
            Native.GetWindowTextW(h, s, s.Capacity);
            w.Title = s.ToString();
            w.Visible = Native.IsWindowVisible(h);
            Native.RECT r;
            w.Rect = Native.GetWindowRect(h, out r) ? new Box(r.Left, r.Top, r.Right, r.Bottom) : new Box(0, 0, 0, 0);
            return w;
        }

        /// <summary>Every top-level window of process <paramref name="pid"/>.</summary>
        public static List<Win> Of(uint pid)
        {
            var list = new List<Win>();
            Native.EnumWindows(delegate (IntPtr h, IntPtr l)
            {
                uint p;
                Native.GetWindowThreadProcessId(h, out p);
                if (p == pid) list.Add(Describe(h));
                return true;
            }, IntPtr.Zero);
            return list;
        }

        /// <summary>
        /// What a window's extended style says about how it behaves, in the
        /// words the app's log uses: out of Alt+Tab and the taskbar (tool
        /// window), never activated, on top, and whether the mouse passes
        /// through it. WS_EX_TRANSPARENT alone only orders painting (the
        /// window is painted after its siblings beneath it); on a layered
        /// window (WS_EX_LAYERED) it makes the mouse pass through.
        /// </summary>
        public static string Style(IntPtr h)
        {
            return StyleWords(Native.GetWindowLongPtrW(h, -20).ToInt64()); // GWL_EXSTYLE
        }

        /// <summary>The words of <see cref="Style"/> for the extended style bits <paramref name="ex"/>.</summary>
        public static string StyleWords(long ex)
        {
            var words = new List<string>();
            if ((ex & 0x00000080) != 0) words.Add("tool window");
            if ((ex & 0x08000000) != 0) words.Add("never active");
            if ((ex & 0x00040000) != 0) words.Add("app window");
            if ((ex & 0x00000008) != 0) words.Add("on top");
            bool layered = (ex & 0x00080000) != 0, transparent = (ex & 0x00000020) != 0;
            if (layered && transparent) words.Add("layered and transparent: clicks pass through");
            else if (layered) words.Add("layered");
            else if (transparent) words.Add("transparent (painted after its siblings)");
            return words.Count == 0 ? "plain" : string.Join(", ", words.ToArray());
        }

        public static string ProgramOf(uint pid)
        {
            try { return Process.GetProcessById((int)pid).ProcessName; }
            catch (Exception) { return "process " + pid; }
        }

        /// <summary>The foreground window: class, title and program.</summary>
        public static string Foreground()
        {
            IntPtr h = Native.GetForegroundWindow();
            if (h == IntPtr.Zero) return "none";
            Win w = Describe(h);
            return string.Format("{0} \"{1}\" ({2})", w.Class, w.Title, ProgramOf(w.Pid));
        }

        /// <summary>The window of process <paramref name="pid"/> with exactly this title, if there is one.</summary>
        public static Win Titled(uint pid, string title)
        {
            foreach (Win w in Of(pid)) if (w.Title == title) return w;
            return null;
        }

        /// <summary>
        /// Where the taskbar puts the tray icon with id <paramref name="uid"/> of window
        /// <paramref name="hwnd"/>; null when it doesn't say.
        /// </summary>
        public static Box TrayIconRect(IntPtr hwnd, uint uid)
        {
            var id = new Native.NOTIFYICONIDENTIFIER();
            id.cbSize = (uint)Marshal.SizeOf(typeof(Native.NOTIFYICONIDENTIFIER));
            id.hWnd = hwnd;
            id.uID = uid;
            Native.RECT r;
            if (Native.Shell_NotifyIconGetRect(ref id, out r) != 0) return null;
            var b = new Box(r.Left, r.Top, r.Right, r.Bottom);
            return b.IsEmpty ? null : b;
        }

        public static Box RectOf(IntPtr h)
        {
            Native.RECT r;
            if (h == IntPtr.Zero || !Native.GetWindowRect(h, out r)) return null;
            return new Box(r.Left, r.Top, r.Right, r.Bottom);
        }

        /// <summary>The part of window <paramref name="h"/> inside its frame, on the desktop.</summary>
        public static Box ClientOf(IntPtr h)
        {
            Native.RECT r;
            if (h == IntPtr.Zero || !Native.GetClientRect(h, out r)) return null;
            var p = new Native.POINT();
            if (!Native.ClientToScreen(h, ref p)) return null;
            return new Box(p.X, p.Y, p.X + r.Right, p.Y + r.Bottom);
        }

        /// <summary>The taskbar's window on the main monitor; zero when there is no taskbar.</summary>
        public static IntPtr Taskbar()
        {
            return Native.FindWindowW("Shell_TrayWnd", null);
        }

        /// <summary>A top-level window of class <paramref name="cls"/>, shown or not; zero when there is none.</summary>
        public static IntPtr OfClass(string cls)
        {
            return Native.FindWindowW(cls, null);
        }
    }

    public static class Desk
    {
        const int SM_XVIRTUALSCREEN = 76, SM_YVIRTUALSCREEN = 77, SM_CXVIRTUALSCREEN = 78, SM_CYVIRTUALSCREEN = 79;

        public static Box Bounds()
        {
            int x = Native.GetSystemMetrics(SM_XVIRTUALSCREEN), y = Native.GetSystemMetrics(SM_YVIRTUALSCREEN);
            return new Box(x, y, x + Native.GetSystemMetrics(SM_CXVIRTUALSCREEN), y + Native.GetSystemMetrics(SM_CYVIRTUALSCREEN));
        }

        /// <summary>Per-monitor DPI awareness for this process, so every coordinate is in physical pixels.</summary>
        public static string BecomeDpiAware()
        {
            // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
            if (Native.SetProcessDpiAwarenessContext(new IntPtr(-4))) return "per-monitor DPI aware";
            return "DPI awareness unchanged (error " + Marshal.GetLastWin32Error() + ")";
        }

        public static string Cursor()
        {
            Native.POINT p;
            return Native.GetCursorPos(out p) ? p.X + "," + p.Y : "unknown";
        }
    }

    /// <summary>A picture of the desktop, kept in memory until it is saved.</summary>
    public sealed class Picture
    {
        public string Name;
        public int Width;
        public int Height;
        public byte[] Bgra;

        const uint SRCCOPY = 0x00CC0020, CAPTUREBLT = 0x40000000, DI_NORMAL = 3, CURSOR_SHOWING = 1;

        /// <summary>The whole desktop now, with the mouse pointer drawn where it is.</summary>
        public static Picture Take(string name)
        {
            Box d = Desk.Bounds();
            if (d.IsEmpty) throw new InvalidOperationException("Windows reports no desktop to take a picture of");
            IntPtr screen = Native.GetDC(IntPtr.Zero);
            if (screen == IntPtr.Zero) throw new InvalidOperationException("no desktop to take a picture of (GetDC failed)");
            IntPtr mem = Native.CreateCompatibleDC(screen);
            var header = new Native.BITMAPINFOHEADER();
            header.biSize = (uint)Marshal.SizeOf(typeof(Native.BITMAPINFOHEADER));
            header.biWidth = d.Width;
            header.biHeight = -d.Height; // top-down rows
            header.biPlanes = 1;
            header.biBitCount = 32;
            IntPtr bits;
            IntPtr dib = Native.CreateDIBSection(screen, ref header, 0, out bits, IntPtr.Zero, 0);
            if (dib == IntPtr.Zero)
            {
                Native.DeleteDC(mem);
                Native.ReleaseDC(IntPtr.Zero, screen);
                throw new InvalidOperationException("couldn't make a bitmap for the picture");
            }
            IntPtr old = Native.SelectObject(mem, dib);
            bool copied = Native.BitBlt(mem, 0, 0, d.Width, d.Height, screen, d.Left, d.Top, SRCCOPY | CAPTUREBLT);
            int error = Marshal.GetLastWin32Error();
            if (copied) DrawPointer(mem, d);
            var pic = new Picture();
            pic.Name = name;
            pic.Width = d.Width;
            pic.Height = d.Height;
            pic.Bgra = new byte[d.Width * d.Height * 4];
            Marshal.Copy(bits, pic.Bgra, 0, pic.Bgra.Length);
            Native.SelectObject(mem, old);
            Native.DeleteObject(dib);
            Native.DeleteDC(mem);
            Native.ReleaseDC(IntPtr.Zero, screen);
            if (!copied) throw new Win32Exception(error, "couldn't copy the desktop into a picture (no desktop this job can see?)");
            return pic;
        }

        static void DrawPointer(IntPtr dc, Box desk)
        {
            var info = new Native.CURSORINFO();
            info.cbSize = (uint)Marshal.SizeOf(typeof(Native.CURSORINFO));
            if (!Native.GetCursorInfo(ref info) || (info.flags & CURSOR_SHOWING) == 0 || info.hCursor == IntPtr.Zero) return;
            int hx = 0, hy = 0;
            Native.ICONINFO icon;
            if (Native.GetIconInfo(info.hCursor, out icon))
            {
                hx = (int)icon.xHotspot;
                hy = (int)icon.yHotspot;
                if (icon.hbmMask != IntPtr.Zero) Native.DeleteObject(icon.hbmMask);
                if (icon.hbmColor != IntPtr.Zero) Native.DeleteObject(icon.hbmColor);
            }
            Native.DrawIconEx(dc, info.ptScreenPos.X - desk.Left - hx, info.ptScreenPos.Y - desk.Top - hy, info.hCursor, 0, 0, 0, IntPtr.Zero, DI_NORMAL);
        }

        public void Save(string folder)
        {
            Png.Save(Path.Combine(folder, Name + ".png"), Width, Height, Bgra);
        }
    }

    /// <summary>A minimal PNG writer: 8-bit RGB, no filtering, one IDAT chunk.</summary>
    public static class Png
    {
        static readonly uint[] CrcTable = MakeCrcTable();

        static uint[] MakeCrcTable()
        {
            var t = new uint[256];
            for (uint n = 0; n < 256; n++)
            {
                uint c = n;
                for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
                t[n] = c;
            }
            return t;
        }

        static uint Crc(byte[] type, byte[] data)
        {
            uint c = 0xFFFFFFFFu;
            foreach (byte b in type) c = CrcTable[(c ^ b) & 0xFF] ^ (c >> 8);
            foreach (byte b in data) c = CrcTable[(c ^ b) & 0xFF] ^ (c >> 8);
            return c ^ 0xFFFFFFFFu;
        }

        static uint Adler32(byte[] data)
        {
            uint a = 1, b = 0;
            foreach (byte x in data)
            {
                a = (a + x) % 65521;
                b = (b + a) % 65521;
            }
            return (b << 16) | a;
        }

        static void BigEndian(Stream s, uint v)
        {
            s.WriteByte((byte)(v >> 24));
            s.WriteByte((byte)(v >> 16));
            s.WriteByte((byte)(v >> 8));
            s.WriteByte((byte)v);
        }

        static void Chunk(Stream s, string type, byte[] data)
        {
            byte[] t = Encoding.ASCII.GetBytes(type);
            BigEndian(s, (uint)data.Length);
            s.Write(t, 0, t.Length);
            s.Write(data, 0, data.Length);
            BigEndian(s, Crc(t, data));
        }

        /// <summary>Write <paramref name="bgra"/> (top-down rows, 4 bytes a pixel) to <paramref name="path"/>.</summary>
        public static void Save(string path, int width, int height, byte[] bgra)
        {
            var raw = new byte[height * (1 + width * 3)];
            int o = 0;
            for (int y = 0; y < height; y++)
            {
                raw[o++] = 0; // filter: none
                int p = y * width * 4;
                for (int x = 0; x < width; x++, p += 4)
                {
                    raw[o++] = bgra[p + 2];
                    raw[o++] = bgra[p + 1];
                    raw[o++] = bgra[p];
                }
            }
            byte[] zlib;
            using (var ms = new MemoryStream())
            {
                ms.WriteByte(0x78); // deflate, 32 KiB window
                ms.WriteByte(0x01); // fastest, no dictionary; (0x78 << 8 | 0x01) % 31 == 0
                using (var deflate = new DeflateStream(ms, CompressionMode.Compress, true))
                {
                    deflate.Write(raw, 0, raw.Length);
                }
                BigEndian(ms, Adler32(raw));
                zlib = ms.ToArray();
            }
            byte[] ihdr;
            using (var h = new MemoryStream())
            {
                BigEndian(h, (uint)width);
                BigEndian(h, (uint)height);
                h.WriteByte(8); // bits per channel
                h.WriteByte(2); // RGB
                h.WriteByte(0);
                h.WriteByte(0);
                h.WriteByte(0);
                ihdr = h.ToArray();
            }
            using (var f = File.Create(path))
            {
                var signature = new byte[] { 137, 80, 78, 71, 13, 10, 26, 10 };
                f.Write(signature, 0, signature.Length);
                Chunk(f, "IHDR", ihdr);
                Chunk(f, "IDAT", zlib);
                Chunk(f, "IEND", new byte[0]);
            }
        }
    }

    /// <summary>Real mouse input, through the same queue as a mouse's.</summary>
    public static class Mouse
    {
        const uint MOVE = 0x0001, LEFTDOWN = 0x0002, LEFTUP = 0x0004, RIGHTDOWN = 0x0008, RIGHTUP = 0x0010, VIRTUALDESK = 0x4000, ABSOLUTE = 0x8000;

        static void Send(uint flags, int dx, int dy)
        {
            var input = new Native.INPUT[1];
            input[0].type = 0; // INPUT_MOUSE
            input[0].mi.dx = dx;
            input[0].mi.dy = dy;
            input[0].mi.dwFlags = flags;
            if (Native.SendInput(1, input, Marshal.SizeOf(typeof(Native.INPUT))) != 1)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SendInput refused the mouse input");
        }

        /// <summary>Move the pointer to (x, y) as a mouse does; where it ended up.</summary>
        public static string MoveTo(int x, int y)
        {
            Box d = Desk.Bounds();
            int nx = (int)Math.Round((x - d.Left) * 65535.0 / Math.Max(1, d.Width - 1));
            int ny = (int)Math.Round((y - d.Top) * 65535.0 / Math.Max(1, d.Height - 1));
            Send(MOVE | ABSOLUTE | VIRTUALDESK, nx, ny);
            Native.POINT p;
            if (Native.GetCursorPos(out p) && (p.X != x || p.Y != y))
            {
                // Rounding of the 0..65535 scale: put it on the exact pixel.
                Native.SetCursorPos(x, y);
            }
            return Desk.Cursor();
        }

        /// <summary>
        /// Press and release the left button at (x, y), <paramref name="times"/> times
        /// in a row (2: a double click), after resting on the spot a moment.
        /// </summary>
        public static string Click(int x, int y, int times)
        {
            string at = MoveTo(x, y);
            Thread.Sleep(150);
            for (int k = 0; k < times; k++)
            {
                if (k > 0) Thread.Sleep(80);
                Send(LEFTDOWN, 0, 0);
                Thread.Sleep(60);
                Send(LEFTUP, 0, 0);
            }
            return at;
        }

        /// <summary>Press and release the right button at (x, y), after resting on the spot a moment.</summary>
        public static string RightClick(int x, int y)
        {
            string at = MoveTo(x, y);
            Thread.Sleep(150);
            Send(RIGHTDOWN, 0, 0);
            Thread.Sleep(60);
            Send(RIGHTUP, 0, 0);
            return at;
        }
    }

    /// <summary>
    /// Pictures and a line about the windows every so often, from a start;
    /// the pictures stay in memory until saved, so the timing holds.
    /// </summary>
    public sealed class Recorder
    {
        readonly Stopwatch clock = Stopwatch.StartNew();
        readonly List<Picture> pictures = new List<Picture>();
        readonly List<string> lines = new List<string>();
        readonly string panelTitle;
        readonly string rootTitle;
        readonly List<string> problems = new List<string>();
        uint pid;
        int count;

        /// <summary>The panel is a flyout (out of Alt+Tab) rather than a small window.</summary>
        public bool PanelIsFlyout = true;

        /// <summary>What went against what the app's windows must be, each once, in the order found.</summary>
        public List<string> Problems { get { return problems; } }

        /// <summary>Note a problem, once.</summary>
        public void Problem(string what)
        {
            foreach (string p in problems) if (p.StartsWith(what)) return;
            problems.Add(what + " (first seen " + DateTime.UtcNow.ToString("HH:mm:ss.fff") + "Z)");
            Note("PROBLEM: " + what);
        }

        const long WS_EX_TOOLWINDOW = 0x00000080, WS_EX_NOACTIVATE = 0x08000000, WS_EX_APPWINDOW = 0x00040000;

        /// <summary>
        /// The styles the shell keeps on its windows, checked on every one
        /// of them that is shown: none an app window (forced onto the
        /// taskbar); the panel as a flyout, the root window and the small
        /// windows tool windows (out of the taskbar and Alt+Tab); the root
        /// window and the small windows never activated. Dialogs and message
        /// boxes are left out: they belong on the taskbar.
        /// </summary>
        void CheckStyles()
        {
            if (pid == 0) return;
            foreach (Win w in Windows.Of(pid))
            {
                if (!w.Visible || w.Class == "#32770") continue;
                string which;
                bool tool, never;
                if (w.Title == panelTitle) { which = "the panel's window"; tool = PanelIsFlyout; never = false; }
                else if (w.Title == rootTitle) { which = "the root window"; tool = true; never = true; }
                else if (w.Title.StartsWith(panelTitle + " confirmation") || w.Title.StartsWith(panelTitle + " notification")) { which = "the small window \"" + w.Title + "\""; tool = true; never = true; }
                else continue;
                long ex = Native.GetWindowLongPtrW(w.Handle, -20).ToInt64();
                if ((ex & WS_EX_APPWINDOW) != 0) Problem(which + " is shown as an app window (forced onto the taskbar)");
                if (tool && (ex & WS_EX_TOOLWINDOW) == 0) Problem(which + " is shown without the tool-window style (in Alt+Tab and the taskbar)");
                if (never && (ex & WS_EX_NOACTIVATE) == 0) Problem(which + " is shown without the never-activate style");
            }
        }

        /// <summary>
        /// A recorder whose lines name the windows titled <paramref name="panel"/>
        /// and <paramref name="root"/> once it follows a process.
        /// </summary>
        public Recorder(string panel, string root)
        {
            panelTitle = panel;
            rootTitle = root;
        }

        /// <summary>The root window: shown or hidden, and its style; "none" when it isn't there.</summary>
        public string Root()
        {
            if (pid == 0) return "(no app yet)";
            Win w = Windows.Titled(pid, rootTitle);
            if (w == null) return "none";
            return (w.Visible ? "shown" : "hidden") + " (" + Windows.Style(w.Handle) + ")";
        }

        /// <summary>From now on, the panel is the window of process <paramref name="appPid"/>.</summary>
        public void Follow(uint appPid)
        {
            pid = appPid;
        }

        public List<string> Lines { get { return lines; } }

        /// <summary>The panel's window: shown or hidden, where; "none" when it isn't there.</summary>
        public string Panel()
        {
            if (pid == 0) return "(no app yet)";
            Win w = Windows.Titled(pid, panelTitle);
            if (w == null) return "none";
            return (w.Visible ? "shown " : "hidden ") + w.Rect + " (" + Windows.Style(w.Handle) + ")";
        }

        public bool PanelShown()
        {
            if (pid == 0) return false;
            Win w = Windows.Titled(pid, panelTitle);
            return w != null && w.Visible && !w.Rect.IsEmpty;
        }

        /// <summary>The panel's window inside its frame, on the desktop; null when it isn't there.</summary>
        public Box PanelClient()
        {
            if (pid == 0) return null;
            Win w = Windows.Titled(pid, panelTitle);
            return w == null ? null : Windows.ClientOf(w.Handle);
        }

        /// <summary>How many of the pictures so far show the panel's window on screen.</summary>
        public int ShownCount;
        public int PictureCount;

        /// <summary>A line for the timeline, with the UTC time the app's log uses.</summary>
        public void Note(string what)
        {
            lines.Add(string.Format("{0} +{1,7:0.000}s  {2}", DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ss.fffZ"), clock.Elapsed.TotalSeconds, what));
        }

        /// <summary>One picture now, and what the windows were.</summary>
        public void Shot(string label)
        {
            count++;
            string name = string.Format("{0:000}-{1}", count, label);
            Picture p = Picture.Take(name);
            pictures.Add(p);
            PictureCount++;
            if (PanelShown()) ShownCount++;
            Note(string.Format("picture {0}: panel {1}; root {2}; foreground {3}; pointer {4}", name, Panel(), Root(), Windows.Foreground(), Desk.Cursor()));
            CheckStyles();
        }

        /// <summary><paramref name="n"/> pictures, one every <paramref name="everyMs"/> milliseconds.</summary>
        public void Burst(string label, int n, int everyMs)
        {
            var start = Stopwatch.StartNew();
            for (int i = 0; i < n; i++)
            {
                long due = (long)i * everyMs;
                long wait = due - start.ElapsedMilliseconds;
                if (wait > 0) Thread.Sleep((int)wait);
                Shot(string.Format("{0}-{1:0000}ms", label, start.ElapsedMilliseconds));
            }
        }

        public void Save(string folder)
        {
            Directory.CreateDirectory(folder);
            foreach (Picture p in pictures) p.Save(folder);
            pictures.Clear();
        }
    }
}
