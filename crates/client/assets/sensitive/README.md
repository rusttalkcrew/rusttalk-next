# Sensitive picture model

`nsfw-mobilenet-v2-int8.onnx` is the MobileNetV2 (width 1.4, 224×224) picture
classifier from [GantMan/nsfw_model 1.2.0](https://github.com/GantMan/nsfw_model/releases/tag/1.2.0),
[MIT](NSFW-MODEL-LICENSE.txt). It gives five scores that add up to 1: drawings,
hentai, neutral, porn and sexy. The authors report about 93% accuracy for their
1.1/1.2 models on their own test set; nobody has measured it on RustTalk's pictures.

The app blurs a picture when hentai + porn is at least `THRESHOLD` in
`src/sensitive.rs`. It leaves "sexy" out on purpose: that class covers swimwear and
similar pictures, and leaving it out avoids false alarms. People can still mark a
picture themselves.

How the file was made:

1. `model.onnx` from [fyko/nsfw v0.3.0](https://github.com/fyko/nsfw/releases/tag/v0.3.0)
   (MIT or Apache-2.0): GantMan's `mobilenet_v2_140_224` converted with
   `tf2onnx --opset 16` by that project's `create_model.yml`. SHA-256
   `0dca163d933af88e03fa0f9d233655857e9cc9f38bfd2bc28ba57c486d70feaf`, 17.3 MB.
2. `python3 quantize.py model.onnx nsfw-mobilenet-v2-int8.onnx` stores the weights
   as int8 with a float scale per channel. That brings the file down to 4.9 MB.
   SHA-256 `2eed8ce87b630fd728e9b755cf880eaea66f1979324d32ee204296258f9b75df`.
   On 150 random self-made pictures, hentai + porn moved by at most 0.09 compared
   with the float model, and by 0.015 on average.

Check the scores on a local folder of pictures with `make sensitive-check DIR=<folder>`.
It runs on this computer and uploads nothing.
