"""生成「文档库分析页改版」布局稿（PIL 绘制，无外部图表库）。"""
from PIL import Image, ImageDraw, ImageFont

W, H = 1600, 1430
BG = "#F5F6F8"
CARD = "#FFFFFF"
BORDER = "#E6E8EC"
T1 = "#1F2329"
T2 = "#5B6472"
T3 = "#8A929E"
BLUE = "#2F6BFF"
GREEN = "#22A06B"
ORANGE = "#F2994A"
RED = "#E5484D"
PURPLE = "#7C5CFF"
TEAL = "#12A5A5"
GRID = "#EEF0F3"

FONT = "C:/Windows/Fonts/msyh.ttc"
FONT_B = "C:/Windows/Fonts/msyhbd.ttc"


def f(size, bold=False):
    return ImageFont.truetype(FONT_B if bold else FONT, size)


img = Image.new("RGB", (W, H), BG)
d = ImageDraw.Draw(img)


def rrect(x, y, w, h, fill=CARD, outline=BORDER, r=12, width=1):
    d.rounded_rectangle([x, y, x + w, y + h], radius=r, fill=fill, outline=outline, width=width)


def txt(x, y, s, size=14, color=T1, bold=False, anchor="la"):
    d.text((x, y), s, font=f(size, bold), fill=color, anchor=anchor)


def card(x, y, w, h, title, sub=None):
    rrect(x, y, w, h)
    txt(x + 18, y + 16, title, 17, T1, True)
    if sub:
        txt(x + 18, y + 42, sub, 12, T3)


def kpi(x, y, w, h, label, value, unit, delta=None, delta_color=GREEN, icon_color=BLUE):
    rrect(x, y, w, h)
    d.rounded_rectangle([x + 16, y + 18, x + 50, y + 52], radius=10, fill=icon_color + "22", outline=icon_color + "55")
    d.ellipse([x + 27, y + 29, x + 39, y + 41], fill=icon_color)
    txt(x + 60, y + 16, label, 12, T2)
    txt(x + 60, y + 32, value, 22, T1, True)
    vw = d.textlength(value, font=f(22, True))
    txt(x + 60 + vw + 4, y + 44, unit, 11, T3)
    if delta:
        txt(x + w - 16, y + 20, delta, 11, delta_color, anchor="ra")


def axis_y(x, y, w, h, ticks=3):
    for i in range(ticks + 1):
        yy = y + h - i * h / ticks
        d.line([x, yy, x + w, yy], fill=GRID, width=1)


# ── 页头 ──
txt(32, 26, "文档库分析", 24, T1, True)
txt(32, 60, "使用视角：你的浏览/编辑节奏 + 知识库被使用情况", 12, T3)

chips = ["今天", "近 7 天", "近 30 天", "全部"]
cx = W - 32
for i, c in enumerate(reversed(chips)):
    tw = int(d.textlength(c, font=f(12))) + 26
    cx -= tw
    active = (c == "近 7 天")
    rrect(cx, 30, tw, 30, fill="#FFFFFF" if not active else BLUE, outline=BORDER if not active else BLUE, r=15)
    txt(cx + tw / 2, 38, c, 12, T2 if not active else "#FFFFFF", anchor="ma")
    cx -= 8
bw = 74
rrect(cx - bw - 10, 30, bw, 30, fill="#FFFFFF", outline=BORDER, r=15)
txt(cx - bw - 10 + bw / 2, 38, "刷新数据", 12, T2, anchor="ma")

# ── KPI ──
ky, kh = 96, 84
gap = 14
kw = (W - 64 - gap * 5) // 6
kpis = [
    ("浏览时长", "3.2", "小时", "▲ 18% vs 上周期", GREEN, BLUE),
    ("打开文件", "47", "个", "▲ 9", GREEN, TEAL),
    ("编辑次数", "126", "次", "▼ 6%", ORANGE, ORANGE),
    ("知识库覆盖率", "38", "%", "被浏览/全部文件", T3, PURPLE),
    ("连续活跃", "5", "天", "最长 12 天", T3, GREEN),
    ("AI 调用", "89", "次", "▲ 12%", GREEN, RED),
]
for i, (label, value, unit, delta, dcolor, icolor) in enumerate(kpis):
    kpi(32 + i * (kw + gap), ky, kw, kh, label, value, unit, delta, dcolor, icolor)

# ── 行 2：趋势 + 时段 ──
y2, h2 = 200, 262
w2 = (W - 64 - 16) // 2
x1 = 32
card(x1, y2, w2, h2, "浏览时长趋势", "柱=每日时长（分钟） · 线=打开次数 · 虚线=7 日均值")
axis_y(x1 + 26, y2 + 78, w2 - 52, h2 - 120)
bars = [42, 55, 38, 61, 48, 72, 58]
bw2 = (w2 - 90) / len(bars)
for i, b in enumerate(bars):
    bh = b / 80 * (h2 - 130)
    bx = x1 + 44 + i * bw2
    d.rounded_rectangle([bx, y2 + 78 + (h2 - 130) - bh, bx + bw2 * 0.52, y2 + 78 + (h2 - 130)], radius=4, fill=BLUE + "B8")
labels = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
for i, lb in enumerate(labels):
    txt(x1 + 44 + i * bw2 + bw2 * 0.26, y2 + h2 - 40, lb, 11, T3, anchor="ma")
pts = [(x1 + 44 + i * bw2 + bw2 * 0.26, y2 + 78 + (h2 - 130) - (bars[i] / 80 * (h2 - 130)) * 1.05) for i in range(len(bars))]
d.line(pts, fill=GREEN, width=2)
for px, py in pts:
    d.ellipse([px - 3, py - 3, px + 3, py + 3], fill=GREEN)

x2 = 32 + w2 + 16
card(x2, y2, w2, h2, "活跃时段分布", "24 小时 · 堆叠：浏览 / 编辑 / 删除 / AI · 峰值 15 时（13 次）")
axis_y(x2 + 26, y2 + 78, w2 - 52, h2 - 120)
hours = [1, 0, 0, 0, 0, 1, 2, 5, 9, 12, 8, 6, 7, 9, 11, 13, 15, 12, 10, 8, 6, 4, 3, 2]
mx = max(hours)
bw3 = (w2 - 90) / 24
for i, v in enumerate(hours):
    bh = v / mx * (h2 - 130)
    bx = x2 + 44 + i * bw3
    base = y2 + 78 + (h2 - 130)
    d.rectangle([bx, base - bh, bx + bw3 * 0.72, base], fill=BLUE + "9E")
    if v >= 12:
        d.rectangle([bx, base - bh, bx + bw3 * 0.72, base - bh * 0.72], fill=ORANGE + "D0")
for hh in (0, 6, 12, 18, 23):
    txt(x2 + 44 + hh * bw3, y2 + h2 - 40, f"{hh}时", 11, T3, anchor="ma")


# ── 行 3：Top10 + 目录 treemap ──
y3, h3 = 478, 300
card(x1, y3, w2, h3, "文件热度 Top 10", "横向条形 · 按浏览时长 · 颜色=所属目录 · 点击直达文件")
names = ["prompt工程.md", "AI应用开发.md", "检索设计.md", "mdgo架构.md", "索引优化.md",
         "RAG笔记.md", "笔记模板.md", "周报.md", "读书清单.md", "临时草稿.md"]
vals = [19.2, 14.6, 11.1, 8.7, 7.4, 5.9, 4.2, 3.1, 2.4, 1.8]
vmax = vals[0]
bh4 = 18
for i, (nm, v) in enumerate(zip(names, vals)):
    by = y3 + 76 + i * (bh4 + 5)
    d.rounded_rectangle([x1 + 190, by, x1 + 190 + (w2 - 260) * v / vmax, by + bh4], radius=3, fill=[BLUE, TEAL, PURPLE, ORANGE][i % 4] + "C0")
    txt(x1 + 18, by + 2, nm, 12, T2)
    txt(x1 + 190 + (w2 - 260) * v / vmax + 8, by + 2, f"{v} 分钟", 11, T3)

card(x2, y3, w2, h3, "目录热度", "矩形树图 · 面积=浏览时长 · 点击目录→筛选左侧榜单")
tx, ty, tw_, th_ = x2 + 18, y3 + 76, w2 - 36, h3 - 100
blocks = [
    (0.00, 0.00, 0.46, 0.62, "AI 笔记", BLUE),
    (0.46, 0.00, 0.54, 0.38, "业务", TEAL),
    (0.46, 0.38, 0.54, 0.24, "docs", PURPLE),
    (0.00, 0.62, 0.30, 0.38, "其他", ORANGE),
    (0.30, 0.62, 0.30, 0.38, "code", GREEN),
    (0.60, 0.62, 0.40, 0.38, "临时", "#9AA4B2"),
]
for rx, ry, rw, rh, label, color in blocks:
    d.rounded_rectangle([tx + tw_ * rx + 2, ty + th_ * ry + 2, tx + tw_ * (rx + rw) - 2, ty + th_ * (ry + rh) - 2],
                        radius=6, fill=color + "33", outline=color + "88")
    txt(tx + tw_ * rx + 10, ty + th_ * ry + 8, label, 10, T2)

# ── 行 4：冷热分布 + 浏览/编辑象限 ──
y4, h4 = 794, 236
card(x1, y4, w2, h4, "文件冷热分布", "按浏览次数分档 · 直接回答“哪些文件从没被读过”")
cx0, cy0, r0 = x1 + 130, y4 + 120, 66
seg = [(0.52, "#C9D3E3", "从未浏览 52%"), (0.24, BLUE, "1 次 24%"), (0.16, TEAL, "2-5 次 16%"), (0.08, ORANGE, ">5 次 8%")]
start = -90
for frac, color, _ in seg:
    d.pieslice([cx0 - r0, cy0 - r0, cx0 + r0, cy0 + r0], start, start + frac * 360, fill=color, outline="#FFFFFF", width=2)
    start += frac * 360
d.ellipse([cx0 - 38, cy0 - 38, cx0 + 38, cy0 + 38], fill=CARD)
txt(cx0, cy0 - 10, "1,284", 16, T1, True, anchor="ma")
txt(cx0, cy0 + 10, "总文件", 10, T3, anchor="ma")
for i, (_, color, label) in enumerate(seg):
    ly = y4 + 74 + i * 26
    d.rounded_rectangle([cx0 + 92, ly + 3, cx0 + 104, ly + 15], radius=3, fill=color)
    txt(cx0 + 112, ly, label, 11, T2)

card(x2, y4, w2, h4, "浏览 vs 编辑", "散点象限 · 右上=核心文件 / 左上=只看不改 / 右下=改了没细看")
qx, qy, qw, qh = x2 + 40, y4 + 70, w2 - 80, h4 - 110
d.rectangle([qx, qy, qx + qw, qy + qh], outline=BORDER)
d.line([qx + qw / 2, qy, qx + qw / 2, qy + qh], fill=GRID, width=1)
d.line([qx, qy + qh / 2, qx + qw, qy + qh / 2], fill=GRID, width=1)
import random
random.seed(7)
for _ in range(46):
    px = qx + random.uniform(0.04, 0.96) * qw
    py = qy + qh - random.uniform(0.04, 0.96) * qh
    d.ellipse([px - 3, py - 3, px + 3, py + 3], fill=BLUE + "88")
txt(qx + 8, qy + 6, "编辑多", 10, T3)
txt(qx + qw - 8, qy + qh - 18, "浏览多", 10, T3, anchor="ra")

# ── 行 5：AI 分析区 ──
y5, h5 = 1046, 300
rrect(32, y5, W - 64, h5, fill="#FBFCFE", outline=BORDER)
txt(52, y5 + 16, "AI 分析", 17, T1, True)
txt(52 + 74, y5 + 20, "生成于 2026/11/15 18:20 · 摘要模型 · 14 段全部成功", 11, T3)
rrect(W - 32 - 130, y5 + 14, 112, 30, fill=BLUE, outline=BLUE, r=15)
txt(W - 32 - 130 + 56, y5 + 22, "重新分析", 12, "#FFFFFF", anchor="ma")

# 结论卡
concl = [("核心结论", "浏览高度集中：Top5 占 68% 时长"), ("风险提示", "52% 文件从未被打开"), ("行动建议", "为 3 个冷目录补索引摘要")]
cw = (W - 64 - 36 - 24) // 3
for i, (tag, body) in enumerate(concl):
    bx = 52 + i * (cw + 12)
    rrect(bx, y5 + 58, cw, 62, fill="#FFFFFF", outline=BORDER, r=10)
    txt(bx + 12, y5 + 68, tag, 10, BLUE)
    txt(bx + 12, y5 + 86, body, 11, T2)

# 双栏手风琴示意
colw = (W - 64 - 36 - 16) // 2
for ci, (label, rows) in enumerate([("行为总结（11 段）", 5), ("知识库总结（3 段）", 3)]):
    bx = 52 + ci * (colw + 16)
    txt(bx, y5 + 132, label, 11, T3, True)
    for ri in range(rows):
        ry = y5 + 152 + ri * 26
        rrect(bx, ry, colw, 22, fill="#FFFFFF", outline=BORDER, r=6)
        d.ellipse([bx + 8, ry + 8, bx + 14, ry + 14], fill=GREEN)
        txt(bx + 20, ry + 4, ["操作记录", "删除记录", "文件增改", "书签", "Git"][ri] if ci == 0 else ["文件与磁盘", "内容分布", "RAG 与生态"][ri], 11, T2)
        'd.polygon([(bx + colw - 20, ry + 8), (bx + colw - 8, ry + 8), (bx + colw - 14, ry + 15)], fill=T3)'

# 页脚说明
txt(32, H - 48, "数据窗口：随顶部时间范围切换（今天 / 近 7 天 / 近 30 天 / 全部） · 数据源：本地统计库 .mdgo/mdgo.db + 扫描缓存 · 空数据显示占位说明而非空白图", 11, T3)

img.save(r"G:\gitProject\mdgo\.review-tmp\behavior-analysis-v2.png")
print("saved")
