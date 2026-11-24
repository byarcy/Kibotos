# Draws the Kibotos icons from logo-source.png (the "K" made of blue blocks):
#
#   kibotos.ico          the app: the logo on a light rounded tile (48 px and
#                        up); the logo alone at small sizes, where a pale tile
#                        would only blur it.
#   types\<ext>.ico      one document icon per archive extension: a page with
#                        a colored band, the extension name and the logo.
#   preview.png          every icon on light and dark backgrounds, to check.
#
# Uses only what Windows includes (GDI+ through .NET), no image libraries.
# Every size is drawn on its own (4x supersampled), then stored as PNG
# inside the .ico. Run from any folder:
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File make_icons.ps1

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Drawing.Imaging;
using System.Drawing.Text;
using System.IO;

public static class KibotosIcons {
    static readonly int[] Sizes = { 16, 20, 24, 32, 40, 48, 64, 256 };
    const int SS = 4;

    // ------------------------------------------------------------ logo

    /// Cuts the blue blocks out of the source picture. The tile behind them
    /// is a soft gradient: it is fitted with a plane per channel from the
    /// pixels around the logo, and every pixel's distance from that plane
    /// becomes its opacity (the color is un-mixed from the background, so
    /// edges keep no gray fringe).
    public static Bitmap ExtractLogo(string path) {
        using (var src = new Bitmap(path)) {
            int x0 = 274 - 10, y0 = 236 - 10, x1 = 738 + 10, y1 = 756 + 6;
            // Least squares fit of bg = a + b*x + c*y over the tile ring.
            double[,] ata = new double[3, 3]; double[][] atb = { new double[3], new double[3], new double[3] };
            for (int y = 190; y < 840; y += 3) {
                for (int x = 200; x < 830; x += 3) {
                    bool nearLogo = x > x0 - 30 && x < x1 + 30 && y > y0 - 30 && y < y1 + 40;
                    if (nearLogo) continue;
                    Color c = src.GetPixel(x, y);
                    double[] v = { 1, x, y };
                    for (int i = 0; i < 3; i++) {
                        for (int j = 0; j < 3; j++) ata[i, j] += v[i] * v[j];
                        atb[0][i] += v[i] * c.R; atb[1][i] += v[i] * c.G; atb[2][i] += v[i] * c.B;
                    }
                }
            }
            double[][] plane = { Solve(ata, atb[0]), Solve(ata, atb[1]), Solve(ata, atb[2]) };
            var logo = new Bitmap(x1 - x0, y1 - y0, PixelFormat.Format32bppArgb);
            for (int y = y0; y < y1; y++) {
                for (int x = x0; x < x1; x++) {
                    Color c = src.GetPixel(x, y);
                    double[] bg = new double[3];
                    for (int k = 0; k < 3; k++) bg[k] = plane[k][0] + plane[k][1] * x + plane[k][2] * y;
                    double dr = c.R - bg[0], dg = c.G - bg[1], db = c.B - bg[2];
                    double dist = Math.Sqrt(dr * dr + dg * dg + db * db);
                    double a = Clamp((dist - 10) / 70.0);
                    // Gray shadow under the blocks: keep it faint.
                    double sat = Math.Max(c.R, Math.Max(c.G, c.B)) - Math.Min(c.R, Math.Min(c.G, c.B));
                    if (sat < 40) a *= sat / 40.0;
                    if (a <= 0.01) { logo.SetPixel(x - x0, y - y0, Color.Transparent); continue; }
                    int r = Byte((bg[0] + dr / a)), g = Byte((bg[1] + dg / a)), b = Byte((bg[2] + db / a));
                    logo.SetPixel(x - x0, y - y0, Color.FromArgb(Byte(a * 255), r, g, b));
                }
            }
            return logo;
        }
    }

    static double[] Solve(double[,] m, double[] v) {
        // 3x3 Gaussian elimination.
        double[,] a = (double[,])m.Clone(); double[] b = (double[])v.Clone();
        for (int i = 0; i < 3; i++) {
            int p = i; for (int r = i + 1; r < 3; r++) if (Math.Abs(a[r, i]) > Math.Abs(a[p, i])) p = r;
            for (int c = 0; c < 3; c++) { double t = a[i, c]; a[i, c] = a[p, c]; a[p, c] = t; }
            double tb = b[i]; b[i] = b[p]; b[p] = tb;
            for (int r = i + 1; r < 3; r++) { double f = a[r, i] / a[i, i]; for (int c = i; c < 3; c++) a[r, c] -= f * a[i, c]; b[r] -= f * b[i]; }
        }
        double[] x = new double[3];
        for (int i = 2; i >= 0; i--) { double s = b[i]; for (int c = i + 1; c < 3; c++) s -= a[i, c] * x[c]; x[i] = s / a[i, i]; }
        return x;
    }

    static double Clamp(double v) { return v < 0 ? 0 : v > 1 ? 1 : v; }
    static int Byte(double v) { return (int)Math.Round(v < 0 ? 0 : v > 255 ? 255 : v); }

    /// The logo cropped to its visible pixels.
    static Bitmap Trim(Bitmap b) {
        int minx = b.Width, miny = b.Height, maxx = 0, maxy = 0;
        for (int y = 0; y < b.Height; y++) for (int x = 0; x < b.Width; x++) if (b.GetPixel(x, y).A > 40) {
            if (x < minx) minx = x; if (x > maxx) maxx = x; if (y < miny) miny = y; if (y > maxy) maxy = y;
        }
        return b.Clone(new Rectangle(minx, miny, maxx - minx + 1, maxy - miny + 1), PixelFormat.Format32bppArgb);
    }

    // ------------------------------------------------------- drawing

    static Graphics Canvas(Bitmap b) {
        var g = Graphics.FromImage(b);
        g.SmoothingMode = SmoothingMode.AntiAlias;
        g.InterpolationMode = InterpolationMode.HighQualityBicubic;
        g.PixelOffsetMode = PixelOffsetMode.HighQuality;
        g.CompositingQuality = CompositingQuality.HighQuality;
        g.TextRenderingHint = TextRenderingHint.AntiAlias;
        return g;
    }

    /// Draws `logo` to fit (keeping proportions) the box, centered.
    static void DrawLogo(Graphics g, Bitmap logo, RectangleF box) {
        float s = Math.Min(box.Width / logo.Width, box.Height / logo.Height);
        float w = logo.Width * s, h = logo.Height * s;
        g.DrawImage(logo, new RectangleF(box.X + (box.Width - w) / 2, box.Y + (box.Height - h) / 2, w, h));
    }

    static GraphicsPath Rounded(RectangleF r, float radius) {
        var p = new GraphicsPath(); float d = radius * 2;
        p.AddArc(r.X, r.Y, d, d, 180, 90); p.AddArc(r.Right - d, r.Y, d, d, 270, 90);
        p.AddArc(r.Right - d, r.Bottom - d, d, d, 0, 90); p.AddArc(r.X, r.Bottom - d, d, d, 90, 90);
        p.CloseFigure(); return p;
    }

    /// Supersampled drawing, reduced to `size`.
    static Bitmap Render(int size, Action<Graphics, float> draw) {
        int big = size * SS;
        using (var hi = new Bitmap(big, big, PixelFormat.Format32bppArgb)) {
            using (var g = Canvas(hi)) { g.Clear(Color.Transparent); draw(g, big); }
            var lo = new Bitmap(size, size, PixelFormat.Format32bppArgb);
            using (var g = Canvas(lo)) {
                g.Clear(Color.Transparent);
                using (var at = new ImageAttributes()) {
                    at.SetWrapMode(WrapMode.TileFlipXY);
                    g.DrawImage(hi, new Rectangle(0, 0, size, size), 0, 0, big, big, GraphicsUnit.Pixel, at);
                }
            }
            return lo;
        }
    }

    public static Bitmap AppIcon(Bitmap logo, int size) {
        return Render(size, (g, s) => {
            if (size < 48) {
                // Small: the logo alone, as large as the square allows.
                float m = s * 0.03f;
                DrawLogo(g, logo, new RectangleF(m, m, s - 2 * m, s - 2 * m));
                return;
            }
            float inset = s * 0.055f;
            var tile = new RectangleF(inset, inset, s - 2 * inset, s - 2 * inset);
            float radius = tile.Width * 0.22f;
            // A soft shadow under the tile.
            using (var shadow = Rounded(new RectangleF(tile.X, tile.Y + s * 0.012f, tile.Width, tile.Height), radius))
            using (var sb = new SolidBrush(Color.FromArgb(38, 30, 41, 59))) g.FillPath(sb, shadow);
            using (var path = Rounded(tile, radius)) {
                using (var fill = new LinearGradientBrush(tile, Color.FromArgb(250, 251, 252), Color.FromArgb(229, 232, 237), 90f)) g.FillPath(fill, path);
                using (var pen = new Pen(Color.FromArgb(70, 120, 130, 145), Math.Max(1f, s / 160f))) g.DrawPath(pen, path);
            }
            float pad = tile.Width * 0.17f;
            DrawLogo(g, logo, new RectangleF(tile.X + pad, tile.Y + pad, tile.Width - 2 * pad, tile.Height - 2 * pad));
        });
    }

    static Color Mix(Color a, Color b, double t) {
        return Color.FromArgb(Byte(a.R + (b.R - a.R) * t), Byte(a.G + (b.G - a.G) * t), Byte(a.B + (b.B - a.B) * t));
    }

    public static Bitmap DocIcon(Bitmap logo, int size, string label, Color band, Color body) {
        return Render(size, (g, s) => {
            // The page: a portrait sheet with its top right corner folded.
            float h = s * 0.94f, w = s * (size <= 24 ? 0.82f : 0.80f);
            float x = (s - w) / 2, y = (s - h) / 2;
            float fold = w * 0.26f, r = s * 0.045f;
            var page = new GraphicsPath();
            page.AddLine(x + r, y, x + w - fold, y);
            page.AddLine(x + w - fold, y, x + w, y + fold);
            page.AddLine(x + w, y + fold, x + w, y + h - r);
            page.AddArc(x + w - 2 * r, y + h - 2 * r, 2 * r, 2 * r, 0, 90);
            page.AddArc(x, y + h - 2 * r, 2 * r, 2 * r, 90, 90);
            page.AddArc(x, y, 2 * r, 2 * r, 180, 90);
            page.CloseFigure();
            float bandH = h * (size <= 24 ? 0.36f : 0.31f);
            var bodyRect = new RectangleF(x, y, w, h);
            using (var fill = new LinearGradientBrush(bodyRect, Mix(body, Color.White, 0.35), body, 90f)) g.FillPath(fill, page);
            // The colored band at the top, clipped to the page.
            g.SetClip(page);
            using (var bb = new LinearGradientBrush(new RectangleF(x, y, w, bandH), Mix(band, Color.White, 0.10), band, 90f))
                g.FillRectangle(bb, x, y, w, bandH);
            g.ResetClip();
            using (var pen = new Pen(Color.FromArgb(110, Mix(band, Color.Black, 0.35)), Math.Max(1f, s / 128f))) g.DrawPath(pen, page);
            // The folded corner.
            var corner = new PointF[] { new PointF(x + w - fold, y), new PointF(x + w - fold, y + fold), new PointF(x + w, y + fold) };
            using (var cb = new SolidBrush(Mix(band, Color.White, 0.45))) g.FillPolygon(cb, corner);
            using (var pen = new Pen(Color.FromArgb(120, Mix(band, Color.Black, 0.35)), Math.Max(1f, s / 128f))) g.DrawPolygon(pen, corner);

            // The extension, when there is room to read it.
            bool text = size >= 40 || (size >= 32 && label.Length <= 3);
            if (text) {
                float room = w - fold - w * 0.14f;
                float em = bandH * 0.70f;
                using (var fam = new FontFamily("Segoe UI")) {
                    Font font = null;
                    SizeF measured = SizeF.Empty;
                    for (; em > 4; em *= 0.94f) {
                        if (font != null) font.Dispose();
                        font = new Font(fam, em, FontStyle.Bold, GraphicsUnit.Pixel);
                        measured = g.MeasureString(label, font, PointF.Empty, StringFormat.GenericTypographic);
                        if (measured.Width <= room) break;
                    }
                    using (font)
                    using (var tb = new SolidBrush(Color.White)) {
                        float tx = x + w * 0.09f;
                        float ty = y + (bandH - measured.Height) / 2;
                        g.DrawString(label, font, tb, tx, ty, StringFormat.GenericTypographic);
                    }
                }
            }
            // The logo in the middle of the page.
            float top = y + bandH, rest = h - bandH;
            float box = Math.Min(w * (size <= 24 ? 0.78f : 0.62f), rest * 0.80f);
            DrawLogo(g, logo, new RectangleF(x + (w - box) / 2, top + (rest - box) / 2, box, box));
        });
    }

    // ----------------------------------------------------------- .ico

    // ---------------------------------------------- 256-color PNG

    /// A 256-color PNG (palette with transparency), reduced by median cut
    /// with Floyd-Steinberg dithering. About a third of the 32-bit PNG for
    /// the 256 px images, which are most of each .ico.
    public static byte[] PalettePng(Bitmap b) {
        int w = b.Width, h = b.Height;
        var px = new int[w * h][];
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) {
            Color c = b.GetPixel(x, y); px[y * w + x] = new int[] { c.R, c.G, c.B, c.A };
        }
        // Median cut over the visible pixels (index 0 is fully transparent).
        var visible = new List<int[]>();
        foreach (var p in px) if (p[3] > 0) visible.Add(p);
        var boxes = new List<List<int[]>> { visible };
        while (boxes.Count < 255) {
            int best = -1, bestRange = 0, bestAxis = 0;
            for (int i = 0; i < boxes.Count; i++) {
                if (boxes[i].Count < 2) continue;
                for (int a = 0; a < 4; a++) {
                    int lo = 255, hi = 0;
                    foreach (var p in boxes[i]) { if (p[a] < lo) lo = p[a]; if (p[a] > hi) hi = p[a]; }
                    // Weight by population so busy boxes split first.
                    int range = (hi - lo) * (int)Math.Sqrt(boxes[i].Count);
                    if (range > bestRange) { bestRange = range; best = i; bestAxis = a; }
                }
            }
            if (best < 0) break;
            var box = boxes[best]; int axis = bestAxis;
            box.Sort((p, q) => p[axis].CompareTo(q[axis]));
            int mid = box.Count / 2;
            boxes[best] = box.GetRange(0, mid);
            boxes.Add(box.GetRange(mid, box.Count - mid));
        }
        var pal = new List<int[]> { new int[] { 0, 0, 0, 0 } };
        foreach (var box in boxes) {
            if (box.Count == 0) continue;
            long r = 0, g = 0, bl = 0, a = 0;
            foreach (var p in box) { r += p[0]; g += p[1]; bl += p[2]; a += p[3]; }
            pal.Add(new int[] { (int)(r / box.Count), (int)(g / box.Count), (int)(bl / box.Count), (int)(a / box.Count) });
        }
        // Map with error diffusion (on color, for opaque-ish pixels).
        var idx = new byte[w * h];
        var err = new double[w * h, 3];
        var cache = new Dictionary<int, int>();
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++) {
            int i = y * w + x; var p = px[i];
            if (p[3] == 0) { idx[i] = 0; continue; }
            int r = Byte(p[0] + err[i, 0]), g = Byte(p[1] + err[i, 1]), bl = Byte(p[2] + err[i, 2]), a = p[3];
            int key = (r << 24) | (g << 16) | (bl << 8) | a;
            int k;
            if (!cache.TryGetValue(key, out k)) {
                long bestD = long.MaxValue; k = 1;
                for (int j = 1; j < pal.Count; j++) {
                    long dr = r - pal[j][0], dg = g - pal[j][1], db = bl - pal[j][2], da = a - pal[j][3];
                    long d = dr * dr + dg * dg + db * db + 2 * da * da;
                    if (d < bestD) { bestD = d; k = j; }
                }
                cache[key] = k;
            }
            idx[i] = (byte)k;
            if (a < 250) continue; // no diffusion across soft edges
            double[] e = { r - pal[k][0], g - pal[k][1], bl - pal[k][2] };
            for (int c = 0; c < 3; c++) {
                if (x + 1 < w) err[i + 1, c] += e[c] * 7 / 16;
                if (y + 1 < h) {
                    if (x > 0) err[i + w - 1, c] += e[c] * 3 / 16;
                    err[i + w, c] += e[c] * 5 / 16;
                    if (x + 1 < w) err[i + w + 1, c] += e[c] * 1 / 16;
                }
            }
        }
        // PNG: IHDR (8-bit palette), PLTE, tRNS, IDAT (zlib), IEND.
        var raw = new MemoryStream();
        for (int y = 0; y < h; y++) { raw.WriteByte(0); raw.Write(idx, y * w, w); }
        byte[] data = raw.ToArray();
        var z = new MemoryStream();
        z.WriteByte(0x78); z.WriteByte(0xDA);
        using (var d = new System.IO.Compression.DeflateStream(z, System.IO.Compression.CompressionLevel.Optimal, true)) d.Write(data, 0, data.Length);
        uint s1 = 1, s2 = 0; foreach (byte v in data) { s1 = (s1 + v) % 65521; s2 = (s2 + s1) % 65521; }
        uint adler = (s2 << 16) | s1;
        z.WriteByte((byte)(adler >> 24)); z.WriteByte((byte)(adler >> 16)); z.WriteByte((byte)(adler >> 8)); z.WriteByte((byte)adler);
        var png = new MemoryStream();
        png.Write(new byte[] { 137, 80, 78, 71, 13, 10, 26, 10 }, 0, 8);
        var ihdr = new MemoryStream(); BE(ihdr, (uint)w); BE(ihdr, (uint)h); ihdr.Write(new byte[] { 8, 3, 0, 0, 0 }, 0, 5);
        Chunk(png, "IHDR", ihdr.ToArray());
        var plte = new byte[pal.Count * 3]; var trns = new byte[pal.Count];
        for (int j = 0; j < pal.Count; j++) { plte[j * 3] = (byte)pal[j][0]; plte[j * 3 + 1] = (byte)pal[j][1]; plte[j * 3 + 2] = (byte)pal[j][2]; trns[j] = (byte)pal[j][3]; }
        Chunk(png, "PLTE", plte); Chunk(png, "tRNS", trns); Chunk(png, "IDAT", z.ToArray()); Chunk(png, "IEND", new byte[0]);
        return png.ToArray();
    }

    static void BE(Stream s, uint v) { s.WriteByte((byte)(v >> 24)); s.WriteByte((byte)(v >> 16)); s.WriteByte((byte)(v >> 8)); s.WriteByte((byte)v); }

    static uint[] crcTable;
    static void Chunk(Stream s, string type, byte[] data) {
        if (crcTable == null) {
            crcTable = new uint[256];
            for (uint n = 0; n < 256; n++) { uint c = n; for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? 0xEDB88320 ^ (c >> 1) : c >> 1; crcTable[n] = c; }
        }
        BE(s, (uint)data.Length);
        byte[] t = System.Text.Encoding.ASCII.GetBytes(type);
        s.Write(t, 0, 4); s.Write(data, 0, data.Length);
        uint crc = 0xFFFFFFFF;
        foreach (byte v in t) crc = crcTable[(crc ^ v) & 0xFF] ^ (crc >> 8);
        foreach (byte v in data) crc = crcTable[(crc ^ v) & 0xFF] ^ (crc >> 8);
        BE(s, crc ^ 0xFFFFFFFF);
    }

    public static void WriteIco(string path, IList<Bitmap> images) {
        var pngs = new List<byte[]>();
        foreach (var b in images) {
            if (b.Width >= 256) { pngs.Add(PalettePng(b)); continue; }
            using (var ms = new MemoryStream()) { b.Save(ms, ImageFormat.Png); pngs.Add(ms.ToArray()); }
        }
        using (var f = new BinaryWriter(File.Create(path))) {
            f.Write((ushort)0); f.Write((ushort)1); f.Write((ushort)images.Count);
            int offset = 6 + 16 * images.Count;
            for (int i = 0; i < images.Count; i++) {
                int s = images[i].Width;
                f.Write((byte)(s >= 256 ? 0 : s)); f.Write((byte)(s >= 256 ? 0 : s));
                f.Write((byte)0); f.Write((byte)0); f.Write((ushort)1); f.Write((ushort)32);
                f.Write(pngs[i].Length); f.Write(offset); offset += pngs[i].Length;
            }
            foreach (var p in pngs) f.Write(p);
        }
    }

    public static List<Bitmap> All(Func<int, Bitmap> make) {
        var list = new List<Bitmap>();
        foreach (int s in Sizes) list.Add(make(s));
        return list;
    }
}
"@

$here = $PSScriptRoot
$logoFull = [KibotosIcons]::ExtractLogo((Join-Path $here "logo-source.png"))
$logoFull.Save((Join-Path $here "logo.png"), [System.Drawing.Imaging.ImageFormat]::Png)

$app = [KibotosIcons]::All({ param($s) [KibotosIcons]::AppIcon($logoFull, $s) })
[KibotosIcons]::WriteIco((Join-Path $here "kibotos.ico"), $app)

# Band and page colors, from the icon sheet. Extensions without their own
# color take their family's.
function Hex($h) { [System.Drawing.ColorTranslator]::FromHtml($h) }
$styles = @{
    zip = "#1860E5", "#E5EBF4"; jar = "#1566DA", "#E8EEF2"; apk = "#0E70C1", "#E8ECEF"; cbz = "#196BCA", "#EBEDEE"
    "7z" = "#307E41", "#D7E2D7"
    rar = "#8F23A7", "#EEE5EF"; cbr = "#7F3AAD", "#ECE8F0"
    tar = "#EE6902", "#F7EBDF"; gz = "#CD4E22", "#F5E0D5"; tgz = "#CD4E22", "#F5E0D5"
    bz2 = "#D9761A", "#F7E8D8"; tbz = "#D9761A", "#F7E8D8"; tbz2 = "#D9761A", "#F7E8D8"
    xz = "#C6340A", "#EFE2DB"; txz = "#C6340A", "#EFE2DB"; lzma = "#B8410E", "#F0E3DB"; tlz = "#B8410E", "#F0E3DB"; lz = "#B8410E", "#F0E3DB"
    zst = "#C32025", "#F2E0DF"; tzst = "#C32025", "#F2E0DF"; lz4 = "#B0174A", "#F2DFE5"; tlz4 = "#B0174A", "#F2DFE5"
    z = "#9C4A2A", "#F1E3DC"; taz = "#9C4A2A", "#F1E3DC"
    iso = "#8E9095", "#EEEEEE"; cab = "#758798", "#EAE9EE"; msi = "#727475", "#EAEAE8"; "001" = "#505252", "#EAEAEA"
}
$types = Join-Path $here "types"
New-Item -ItemType Directory -Force $types | Out-Null
$previews = @()
foreach ($ext in ($styles.Keys | Sort-Object)) {
    $band = Hex $styles[$ext][0]; $body = Hex $styles[$ext][1]
    $label = $ext.ToUpperInvariant()
    $imgs = [KibotosIcons]::All({ param($s) [KibotosIcons]::DocIcon($logoFull, $s, $label, $band, $body) })
    [KibotosIcons]::WriteIco((Join-Path $types "$ext.ico"), $imgs)
    $previews += , @($ext, $imgs)
}

# Preview: each icon at 256, 48, 32, 24 and 16 px, on light and dark.
$rows = $previews.Count + 1
$cell = 270; $w = 2 * (256 + 48 + 32 + 24 + 16 + 60) + 40
$sheet = New-Object System.Drawing.Bitmap $w, ($rows * $cell)
$g = [System.Drawing.Graphics]::FromImage($sheet)
$g.Clear([System.Drawing.Color]::White)
$half = $w / 2
$g.FillRectangle((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(32, 32, 32))), $half, 0, $half, $rows * $cell)
$all = @(, @("app", $app)) + $previews
for ($r = 0; $r -lt $all.Count; $r++) {
    foreach ($side in 0, 1) {
        $x = 20 + $side * $half
        foreach ($i in 7, 5, 3, 2, 0) {
            $img = $all[$r][1][$i]
            $g.DrawImage($img, $x, $r * $cell + 7, $img.Width, $img.Height)
            $x += $img.Width + 14
        }
    }
}
$g.Dispose()
$sheet.Save((Join-Path $here "preview.png"), [System.Drawing.Imaging.ImageFormat]::Png)
"kibotos.ico: {0:N0} bytes" -f (Get-Item (Join-Path $here "kibotos.ico")).Length
"types: {0} icons, {1:N0} bytes" -f (Get-ChildItem $types -Filter *.ico).Count, ((Get-ChildItem $types -Filter *.ico | Measure-Object Length -Sum).Sum)
