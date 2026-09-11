#!/usr/bin/env python3
"""
Vellum app icon — 泥金首字母（illuminated initial）。

繕寫本每一章以一個放大的裝飾首字母開頭，周圍的正文則維持樸素。
這個 icon 就是那個結構：

  - 暖色羊皮紙底        承載文字的那一層，也就是這個 proxy 在做的事
  - 三條淡界欄          scribes 寫字前先劃的格線
  - 一個寬頭筆 V        首字母。粗細變化來自固定筆尖角度，不是對稱漸縮

寬頭筆（broad nib）的原理：筆尖是一段固定角度的線段，不是圓點。
筆畫方向與筆尖夾角接近 90° 時最粗，平行時最細 —— 這是 V 左粗右細的來源，
也是它看起來像「寫的」而不是「畫的」的原因。

所有尺寸從 8 倍超取樣降下來。

用法：
    python make_icon.py            產生 _master.png（1024px）
    npx tauri icon src-tauri/icons/_master.png    由官方工具產生整套
    rm -rf src-tauri/target/release/build/vellum-*   強制重新嵌入

最後一步不能省。tauri-build 的 build script 只宣告
`cargo:rerun-if-changed=tauri.conf.json`，換了 icon 檔不會讓它失效，
`cargo clean -p vellum` 也清不掉已產生的 resource —— exe 會繼續帶著舊圖。

python make_icon.py preview  另外產生亮/暗底的尺寸對照圖
"""
import math
import sys
from PIL import Image, ImageDraw, ImageFilter

SS = 8

# --- 品牌色（對應 src/styles/tokens.css）---
CREAM_TL = (248, 241, 227)   # 左上，暖奶白
VELLUM_BR = (229, 206, 174)  # 右下，較深的羊皮色
RIM = (206, 179, 143)        # 內緣，淺底在淺色工作列上仍要有輪廓
RULE = (198, 162, 123)       # --apricot-deep，界欄
INK = (98, 80, 158)          # --lavender-deep 壓深，墨色

NIB_ANGLE = math.radians(-35)  # 寬頭筆握持角度


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def nib_ribbon(draw, pts, size, nib_w, color, alpha=255):
    """沿著 pts 用固定角度的筆尖掃出一條帶狀筆畫。"""
    half = nib_w / 2
    ox, oy = math.cos(NIB_ANGLE) * half, math.sin(NIB_ANGLE) * half
    for i in range(len(pts) - 1):
        x0, y0 = pts[i][0] * size, pts[i][1] * size
        x1, y1 = pts[i + 1][0] * size, pts[i + 1][1] * size
        draw.polygon(
            [
                (x0 - ox, y0 - oy),
                (x0 + ox, y0 + oy),
                (x1 + ox, y1 + oy),
                (x1 - ox, y1 - oy),
            ],
            fill=(*color, alpha),
        )


def lerp_pts(a, b, steps):
    return [
        (a[0] + (b[0] - a[0]) * i / steps, a[1] + (b[1] - a[1]) * i / steps)
        for i in range(steps + 1)
    ]


def v_strokes():
    """V 的兩筆。左筆往右下（與筆尖近垂直 → 粗），右筆往右上（→ 細）。"""
    apex = (0.500, 0.715)
    left = lerp_pts((0.288, 0.272), apex, 40)
    right = lerp_pts(apex, (0.712, 0.272), 40)
    return left, right


def render(size):
    S = size * SS
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # --- 羊皮紙底 ---
    # 漸層對 (x+y) 是線性的，所以小圖雙線性放大等價於逐像素計算，
    # 但快了三個數量級（1024px @ 8x 超取樣 = 6700 萬次迴圈）。
    G = 64
    small = Image.new("RGBA", (G, G))
    spx = small.load()
    for y in range(G):
        for x in range(G):
            spx[x, y] = (*lerp(CREAM_TL, VELLUM_BR, (x + y) / (2 * (G - 1))), 255)
    img.alpha_composite(small.resize((S, S), Image.BILINEAR))

    # --- 界欄：長度不等，避開「三條橫線 = 選單」的誤讀 ---
    rules = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    rd = ImageDraw.Draw(rules)
    lw = max(1, int(S * 0.020))
    # 界欄貫穿整個文字區塊並從字母後面通過 —— 停在半途會看起來像畫壞了
    for y_frac, op in [(0.300, 92), (0.470, 108), (0.640, 92)]:
        x0, x1 = 0.155, 0.845
        y = y_frac * S
        rd.rounded_rectangle(
            [x0 * S, y - lw / 2, x1 * S, y + lw / 2], radius=lw / 2, fill=(*RULE, op)
        )
    img.alpha_composite(rules)

    left, right = v_strokes()
    nib = S * 0.146

    # --- 墨在羊皮上的滲色：極淡、貼著筆畫，不是投影 ---
    bleed = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    bd = ImageDraw.Draw(bleed)
    nib_ribbon(bd, left, S, nib * 1.18, INK, 60)
    nib_ribbon(bd, right, S, nib * 1.18, INK, 60)
    img.alpha_composite(bleed.filter(ImageFilter.GaussianBlur(S * 0.012)))

    # --- 首字母 ---
    ink = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    idraw = ImageDraw.Draw(ink)
    nib_ribbon(idraw, left, S, nib, INK)
    nib_ribbon(idraw, right, S, nib, INK)
    img.alpha_composite(ink)

    # --- 內緣 ---
    radius = S * 0.222
    rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle(
        [0, 0, S - 1, S - 1],
        radius=radius,
        outline=(*RIM, 145),
        width=max(1, int(S * 0.011)),
    )
    img.alpha_composite(rim)

    # --- 圓角遮罩 ---
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, S - 1, S - 1], radius=radius, fill=255)
    img.putalpha(mask)

    return img.resize((size, size), Image.LANCZOS)


def main():
    render(1024).save("_master.png")
    print("wrote _master.png (1024px)")
    print("next: npx tauri icon src-tauri/icons/_master.png")
    print("then: rm -rf src-tauri/target/release/build/vellum-*")

    if "preview" in sys.argv:
        sizes = [16, 24, 32, 48, 64, 128, 256]
        pad, gap = 28, 22
        w = pad * 2 + sum(sizes) + gap * (len(sizes) - 1)
        h = pad * 2 + max(sizes)
        for bg, out in [
            ((28, 30, 40), "_preview-dark.png"),
            ((246, 243, 236), "_preview-light.png"),
        ]:
            sheet = Image.new("RGBA", (w, h), (*bg, 255))
            x = pad
            for s in sizes:
                sheet.alpha_composite(render(s), (x, pad + (max(sizes) - s) // 2))
                x += s + gap
            sheet.save(out)
            print("wrote", out)


if __name__ == "__main__":
    main()
