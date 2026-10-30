"""IronAV sidecar: object detection with a YOLOv8 model exported to ONNX, run by ONNX Runtime.

    pip install onnxruntime numpy        (onnxruntime-gpu for CUDA)
    yolo export model=yolov8n.pt format=onnx      (with the ultralytics package, once)

Set YOLO_MODEL to the .onnx file (default yolov8n.onnx next to this script); YOLO_SCORE is the lowest score kept
(default 0.4). List it in the IRONAV_SIDECARS file:

    {"detect": {"command": ["python", "/path/to/yolo_detect.py"]}}

IronAV sends one picture a second, 640 wide, as raw RGB. Each picture with something in it becomes an `objects`
observation: labels, scores and boxes as fractions of the picture (x0, y0, x1, y1).
Protocol: docs/sidecars.md. Not run by IronAV's tests.
"""
import base64
import json
import os
import sys

import numpy as np
import onnxruntime as ort

COCO = ("person bicycle car motorcycle airplane bus train truck boat traffic_light fire_hydrant stop_sign "
        "parking_meter bench bird cat dog horse sheep cow elephant bear zebra giraffe backpack umbrella handbag tie "
        "suitcase frisbee skis snowboard sports_ball kite baseball_bat baseball_glove skateboard surfboard "
        "tennis_racket bottle wine_glass cup fork knife spoon bowl banana apple sandwich orange broccoli carrot "
        "hot_dog pizza donut cake chair couch potted_plant bed dining_table toilet tv laptop mouse remote keyboard "
        "cell_phone microwave oven toaster sink refrigerator book clock vase scissors teddy_bear hair_drier "
        "toothbrush").split()
SIZE = 640


def say(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def letterbox(rgb):
    """The picture scaled (nearest) to fit SIZE x SIZE, padded with grey; returns it and the scale."""
    h, w, _ = rgb.shape
    s = SIZE / max(w, h)
    nw, nh = max(1, round(w * s)), max(1, round(h * s))
    ys = (np.arange(nh) / s).astype(int).clip(0, h - 1)
    xs = (np.arange(nw) / s).astype(int).clip(0, w - 1)
    out = np.full((SIZE, SIZE, 3), 114, np.uint8)
    out[:nh, :nw] = rgb[ys][:, xs]
    return out, s


def nms(boxes, scores, iou=0.5):
    order = scores.argsort()[::-1]
    keep = []
    while order.size:
        i = order[0]
        keep.append(i)
        xx0 = np.maximum(boxes[i, 0], boxes[order[1:], 0])
        yy0 = np.maximum(boxes[i, 1], boxes[order[1:], 1])
        xx1 = np.minimum(boxes[i, 2], boxes[order[1:], 2])
        yy1 = np.minimum(boxes[i, 3], boxes[order[1:], 3])
        inter = np.clip(xx1 - xx0, 0, None) * np.clip(yy1 - yy0, 0, None)
        area = lambda b: (b[..., 2] - b[..., 0]) * (b[..., 3] - b[..., 1])
        order = order[1:][inter / (area(boxes[i]) + area(boxes[order[1:]]) - inter + 1e-9) < iou]
    return keep


def detect(session, rgb, min_score):
    h, w, _ = rgb.shape
    img, s = letterbox(rgb)
    x = img.astype(np.float32).transpose(2, 0, 1)[None] / 255.0
    pred = session.run(None, {session.get_inputs()[0].name: x})[0][0]  # (84, 8400): cx, cy, w, h, 80 scores
    pred = pred.T
    cls = pred[:, 4:].argmax(1)
    score = pred[np.arange(len(pred)), 4 + cls]
    m = score >= min_score
    if not m.any():
        return []
    p, cls, score = pred[m], cls[m], score[m]
    boxes = np.stack([p[:, 0] - p[:, 2] / 2, p[:, 1] - p[:, 3] / 2, p[:, 0] + p[:, 2] / 2, p[:, 1] + p[:, 3] / 2], 1) / s
    found = []
    for c in np.unique(cls):
        idx = np.where(cls == c)[0]
        for i in nms(boxes[idx], score[idx]):
            b = boxes[idx[i]]
            found.append({
                "label": COCO[c] if c < len(COCO) else str(int(c)),
                "score": round(float(score[idx[i]]), 3),
                "box": [round(float(np.clip(b[0] / w, 0, 1)), 4), round(float(np.clip(b[1] / h, 0, 1)), 4),
                        round(float(np.clip(b[2] / w, 0, 1)), 4), round(float(np.clip(b[3] / h, 0, 1)), 4)],
            })
    return sorted(found, key=lambda o: -o["score"])


def main():
    model = os.environ.get("YOLO_MODEL", os.path.join(os.path.dirname(os.path.abspath(__file__)), "yolov8n.onnx"))
    min_score = float(os.environ.get("YOLO_SCORE", "0.4"))
    session = ort.InferenceSession(model, providers=ort.get_available_providers())
    say({"op": "hello", "protocol": 1, "wants": {"video": {"every_seconds": 1, "width": SIZE, "format": "rgb24"}}})
    for line in sys.stdin:
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get("op") == "video":
            rgb = np.frombuffer(base64.b64decode(msg["data"]), np.uint8).reshape(msg["height"], msg["width"], 3)
            found = detect(session, rgb, min_score)
            if found:
                say({"op": "observe", "kind": "objects", "t": msg["t"], "data": {"count": len(found), "objects": found}})
        elif msg.get("op") == "end":
            break


if __name__ == "__main__":
    main()
