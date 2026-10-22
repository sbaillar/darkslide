# NAFNet, "Simple Baselines for Image Restoration" (Chen, Chu, Zhang, Sun, ECCV 2022).
# Architecture of megvii-research/NAFNet@2b4af71ebe098a92a75910c233a3965a3e93ede4
# (basicsr/models/archs/NAFNet_arch.py), Copyright (c) 2022 megvii-model, MIT licence
# (LICENSE-NAFNet). Rewritten for tracing: a plain LayerNorm2d instead of the custom
# autograd function (same forward), no padding to a multiple of 16 (tiles are 512 px),
# no training-only dropout. State-dict names match the official checkpoints.
import torch
import torch.nn as nn

# Every LayerNorm's input is divided by this first. Exact in real arithmetic (LayerNorm is
# scale-invariant; eps is scaled to match), but it keeps the variance of the widest
# activations (|x| ~ 500 on real photos) inside float16: (x/16)^2 < 1024 instead of 250 000.
LN_SCALE = 16.0


class LayerNorm2d(nn.Module):
    def __init__(self, c, eps=1e-6):
        super().__init__()
        self.weight = nn.Parameter(torch.ones(c))
        self.bias = nn.Parameter(torch.zeros(c))
        self.eps = eps

    def forward(self, x):
        x = x / LN_SCALE
        mu = x.mean(1, keepdim=True)
        d = x - mu
        var = (d * d).mean(1, keepdim=True)
        y = d / torch.sqrt(var + self.eps / (LN_SCALE * LN_SCALE))
        return self.weight.view(1, -1, 1, 1) * y + self.bias.view(1, -1, 1, 1)


class NAFBlock(nn.Module):
    def __init__(self, c):
        super().__init__()
        dw = c * 2
        self.conv1 = nn.Conv2d(c, dw, 1)
        self.conv2 = nn.Conv2d(dw, dw, 3, padding=1, groups=dw)
        self.conv3 = nn.Conv2d(dw // 2, c, 1)
        self.sca = nn.Sequential(nn.AdaptiveAvgPool2d(1), nn.Conv2d(dw // 2, dw // 2, 1))
        self.conv4 = nn.Conv2d(c, 2 * c, 1)
        self.conv5 = nn.Conv2d(c, c, 1)
        self.norm1 = LayerNorm2d(c)
        self.norm2 = LayerNorm2d(c)
        self.beta = nn.Parameter(torch.zeros((1, c, 1, 1)))
        self.gamma = nn.Parameter(torch.zeros((1, c, 1, 1)))

    def forward(self, inp):
        x = self.conv2(self.conv1(self.norm1(inp)))
        a, b = x.chunk(2, dim=1)  # SimpleGate
        x = a * b
        x = self.conv3(x * self.sca(x))  # simplified channel attention
        y = inp + x * self.beta
        a, b = self.conv4(self.norm2(y)).chunk(2, dim=1)
        return y + self.conv5(a * b) * self.gamma


class NAFNet(nn.Module):
    """NAFNet-SIDD-width32: width 32, encoders (2, 2, 4, 8), 12 middle blocks, decoders 2 each."""

    def __init__(self, width=32, enc=(2, 2, 4, 8), middle=12, dec=(2, 2, 2, 2)):
        super().__init__()
        self.intro = nn.Conv2d(3, width, 3, padding=1)
        self.ending = nn.Conv2d(width, 3, 3, padding=1)
        self.encoders, self.decoders = nn.ModuleList(), nn.ModuleList()
        self.ups, self.downs = nn.ModuleList(), nn.ModuleList()
        ch = width
        for n in enc:
            self.encoders.append(nn.Sequential(*[NAFBlock(ch) for _ in range(n)]))
            self.downs.append(nn.Conv2d(ch, 2 * ch, 2, 2))
            ch *= 2
        self.middle_blks = nn.Sequential(*[NAFBlock(ch) for _ in range(middle)])
        for n in dec:
            self.ups.append(nn.Sequential(nn.Conv2d(ch, ch * 2, 1, bias=False), nn.PixelShuffle(2)))
            ch //= 2
            self.decoders.append(nn.Sequential(*[NAFBlock(ch) for _ in range(n)]))

    def forward(self, inp):  # (N, 3, H, W), sRGB-encoded 0..1, H and W multiples of 16
        x = self.intro(inp)
        skips = []
        for enc, down in zip(self.encoders, self.downs):
            x = enc(x)
            skips.append(x)
            x = down(x)
        x = self.middle_blks(x)
        for dec, up, skip in zip(self.decoders, self.ups, skips[::-1]):
            x = dec(up(x) + skip)
        return self.ending(x) + inp
