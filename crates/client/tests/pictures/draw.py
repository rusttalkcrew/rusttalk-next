"""Self-made, ordinary test pictures for tests/sensitive_model.rs: a landscape, a
pizza, a bowl of fruit and a cartoon face, drawn from shapes (CC0, made for RustTalk).

    python3 draw.py crates/client/tests/pictures

Needs Pillow.
"""
import sys, random
from PIL import Image, ImageDraw, ImageFilter
out = sys.argv[1]
W, H = 320, 240
def landscape():
    im = Image.new("RGB", (W, H)); d = ImageDraw.Draw(im)
    for y in range(H):
        t = y / H
        d.line([(0, y), (W, y)], fill=(int(90 + 100*t), int(150 + 80*t), 235))
    d.ellipse([230, 30, 280, 80], fill=(255, 220, 90))
    d.polygon([(0, 170), (60, 100), (130, 160), (200, 90), (320, 170), (320, 240), (0, 240)], fill=(110, 120, 130))
    d.polygon([(170, 120), (200, 90), (230, 120)], fill=(240, 240, 245))
    d.rectangle([0, 180, W, H], fill=(70, 140, 60))
    for x in range(10, W, 40):
        d.rectangle([x+8, 190, x+12, 215], fill=(90, 60, 30)); d.ellipse([x, 165, x+20, 200], fill=(30, 100, 40))
    return im.filter(ImageFilter.GaussianBlur(0.8))
def food():
    im = Image.new("RGB", (W, H), (200, 170, 130)); d = ImageDraw.Draw(im)
    for x in range(0, W, 16): d.line([(x, 0), (x, H)], fill=(185, 155, 115), width=3)
    d.ellipse([60, 20, 260, 220], fill=(245, 245, 240))
    d.ellipse([80, 40, 240, 200], fill=(215, 150, 60))
    d.ellipse([90, 50, 230, 190], fill=(200, 50, 35))
    random.seed(3)
    for _ in range(14):
        x, y = random.randint(105, 205), random.randint(65, 165)
        d.ellipse([x, y, x+16, y+16], fill=(150, 30, 30))
    for _ in range(25):
        x, y = random.randint(100, 215), random.randint(60, 175)
        d.ellipse([x, y, x+7, y+5], fill=(60, 140, 50))
    return im.filter(ImageFilter.GaussianBlur(0.6))
def cartoon():
    im = Image.new("RGB", (W, H), (120, 200, 250)); d = ImageDraw.Draw(im)
    d.ellipse([90, 40, 230, 180], fill=(255, 210, 0), outline=(0, 0, 0), width=4)
    d.ellipse([125, 80, 145, 105], fill=(0, 0, 0)); d.ellipse([175, 80, 195, 105], fill=(0, 0, 0))
    d.arc([120, 90, 200, 155], 20, 160, fill=(0, 0, 0), width=5)
    d.rectangle([0, 200, W, H], fill=(80, 180, 80))
    d.text((10, 10), "Hello!", fill=(0, 0, 0))
    return im
def fruit():
    im = Image.new("RGB", (W, H), (240, 235, 225)); d = ImageDraw.Draw(im)
    d.pieslice([40, 90, 280, 260], 180, 360, fill=(120, 80, 50))
    for (x, y, c) in [(80, 100, (230, 40, 40)), (130, 85, (250, 160, 20)), (180, 100, (120, 190, 40)), (115, 125, (240, 220, 40)), (170, 130, (150, 40, 120))]:
        d.ellipse([x, y, x+55, y+55], fill=c)
    return im
for name, f in [("landscape", landscape), ("food", food), ("cartoon", cartoon), ("fruit", fruit)]:
    f().save(f"{out}/{name}.png", optimize=True)
