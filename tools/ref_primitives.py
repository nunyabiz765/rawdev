#!/usr/bin/env python3
"""ref_primitives.py - reference outputs of every OpenCV / Pillow primitive the recipes use, computed by
the real cv2 + PIL, so rawdev's Rust ports can be checked number-for-number (`cargo test --release -- --ignored`).

    tools/ref_primitives.py STAGE.bin WIDTH HEIGHT

STAGE.bin is any frame's tone stage from `rawdev develop --dump-stages` (OUT/stages/<name>/s1_tone.bin;
WIDTH/HEIGHT = the frame's size). A 600x400 crop from its centre plus synthetic grids are the inputs.
Writes ONLY to <repo>/scratch/prim/. Needs numpy, opencv-python (cv2) and Pillow.
"""
import sys
from pathlib import Path
import cv2, numpy as np
from PIL import Image, ImageFilter

P = Path(__file__).resolve().parent.parent / 'scratch'
O = P / 'prim'; O.mkdir(parents=True, exist_ok=True)
cv2.setNumThreads(4)

def save(name, a):
    np.ascontiguousarray(a).tofile(O / f'{name}.bin'); print(name, a.shape, a.dtype)

# every 8-bit triple, index = (c0 << 16) | (c1 << 8) | c2
v = np.arange(1 << 24, dtype=np.uint32)
grid = np.stack([(v >> 16) & 255, (v >> 8) & 255, v & 255], -1).astype(np.uint8).reshape(4096, 4096, 3)
save('lab_srgb', cv2.cvtColor(grid, cv2.COLOR_RGB2LAB))                       # grid = RGB
save('lab_lin', cv2.cvtColor(np.ascontiguousarray(grid[..., ::-1]), cv2.COLOR_LBGR2Lab))   # grid = RGB, fed as BGR
save('rgb_from_lab_srgb', cv2.cvtColor(grid, cv2.COLOR_LAB2RGB))              # grid = Lab
save('rgb_from_lab_lin', np.ascontiguousarray(cv2.cvtColor(grid, cv2.COLOR_Lab2LBGR)[..., ::-1]))
save('ycc', cv2.cvtColor(np.ascontiguousarray(grid[..., ::-1]), cv2.COLOR_BGR2YCrCb))   # grid = RGB
save('rgb_from_ycc', cv2.cvtColor(grid, cv2.COLOR_YCrCb2RGB))                 # grid = YCrCb

W, H = int(sys.argv[2]), int(sys.argv[3])
s1 = np.fromfile(sys.argv[1], np.uint8).reshape(H, W, 3)
y0, x0 = H // 2 - 200, W // 2 - 300
crop = np.ascontiguousarray(s1[y0:y0 + 400, x0:x0 + 600]); save('crop', crop)        # 600x400 RGB
save('median3', cv2.medianBlur(crop, 3))
save('median5', cv2.medianBlur(np.ascontiguousarray(crop[..., 1]), 5))
yy = crop @ np.array([.2126, .7152, .0722], np.float32)
core = (yy > 160).astype(np.uint8); save('core', core)
save('dilate15', cv2.dilate(core, cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (15, 15))))
f = (crop[..., 0].astype(np.float32) / 255) ** 2; save('gauss_in', f)
save('gauss15', cv2.GaussianBlur(f, (0, 0), 1.5))
bgr = cv2.cvtColor(crop, cv2.COLOR_RGB2BGR)
save('nlm_gray', cv2.fastNlMeansDenoising(np.ascontiguousarray(crop[..., 1]), None, 6, 7, 21))
lab = cv2.cvtColor(bgr, cv2.COLOR_LBGR2Lab)
save('nlm_ab', cv2.fastNlMeansDenoising(np.ascontiguousarray(lab[..., 1:]), None, 9, 7, 21))
save('nlm_color', cv2.cvtColor(cv2.fastNlMeansDenoisingColored(bgr, None, 6, 9, 7, 21), cv2.COLOR_BGR2RGB))
im = Image.fromarray(crop)
save('usm', np.asarray(im.filter(ImageFilter.UnsharpMask(radius=1.0, percent=45, threshold=4))))
save('boxblur', np.asarray(im.filter(ImageFilter.GaussianBlur(1.0))))
t = im.copy(); t.thumbnail((200, 200), Image.LANCZOS); save('lanczos', np.asarray(t))
save('reduce3', np.asarray(im.reduce(3)))
t = im.copy(); t.thumbnail((80, 80)); save('thumb_bicubic', np.asarray(t))   # reduce(3) then bicubic, like the camera JPEG
