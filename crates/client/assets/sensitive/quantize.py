"""Shrink the NSFW picture model for the app (see README.md): weight-only int8.

Each large convolution or matrix weight becomes int8 plus one float scale per output
channel, turned back into floats by Cast and Mul nodes that the runtime folds into
constants when it loads the model. Depthwise 3x3 kernels have only nine weights per
channel, too few to quantize well, so they stay float. The model still computes in
float; only the file is smaller.

    python3 quantize.py model.onnx nsfw-mobilenet-v2-int8.onnx

Needs the `onnx` and `numpy` Python packages.
"""

import sys

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

src, dst = sys.argv[1], sys.argv[2]
model = onnx.load(src)
graph = model.graph
keep, nodes = [], []
for tensor in graph.initializer:
    w = numpy_helper.to_array(tensor)
    depthwise = w.ndim == 4 and w.shape[1] == 1
    if w.dtype != np.float32 or w.size < 1024 or w.ndim < 2 or depthwise:
        keep.append(tensor)
        continue
    # Conv weights are OIHW: one scale per output channel. MatMul's are KxN: per column.
    axis = 0 if w.ndim == 4 else 1
    others = tuple(i for i in range(w.ndim) if i != axis)
    scale = np.abs(w).max(axis=others, keepdims=True) / 127.0
    scale[scale == 0] = 1.0
    q = np.clip(np.round(w / scale), -127, 127).astype(np.int8)
    name = tensor.name
    keep.append(numpy_helper.from_array(q, name + "_q"))
    keep.append(numpy_helper.from_array(scale.astype(np.float32), name + "_s"))
    nodes.append(helper.make_node("Cast", [name + "_q"], [name + "_f"], to=TensorProto.FLOAT))
    nodes.append(helper.make_node("Mul", [name + "_f", name + "_s"], [name]))
del graph.initializer[:]
graph.initializer.extend(keep)
rest = list(graph.node)
del graph.node[:]
graph.node.extend(nodes + rest)
onnx.checker.check_model(model)
onnx.save(model, dst)
