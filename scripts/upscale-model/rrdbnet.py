# RRDBNet, the generator of Real-ESRGAN (Wang, Xie, Dong, Shan, "Real-ESRGAN: Training
# Real-World Blind Super-Resolution with Pure Synthetic Data", ICCVW 2021).
# Architecture of xinntao/BasicSR (basicsr/archs/rrdbnet_arch.py, Apache-2.0), as used by
# xinntao/Real-ESRGAN (BSD-3-Clause, LICENSE-Real-ESRGAN) for RealESRGAN_x2plus.
# Rewritten for tracing: the x2 model's pixel unshuffle is a fixed strided convolution
# (one 0/1 weight per output channel, exact) instead of a 6-D reshape, so Core ML keeps it
# on the Neural Engine. State-dict names match the official checkpoints.
import torch
import torch.nn as nn
import torch.nn.functional as F


class ResidualDenseBlock(nn.Module):
    def __init__(self, num_feat=64, num_grow_ch=32):
        super().__init__()
        self.conv1 = nn.Conv2d(num_feat, num_grow_ch, 3, 1, 1)
        self.conv2 = nn.Conv2d(num_feat + num_grow_ch, num_grow_ch, 3, 1, 1)
        self.conv3 = nn.Conv2d(num_feat + 2 * num_grow_ch, num_grow_ch, 3, 1, 1)
        self.conv4 = nn.Conv2d(num_feat + 3 * num_grow_ch, num_grow_ch, 3, 1, 1)
        self.conv5 = nn.Conv2d(num_feat + 4 * num_grow_ch, num_feat, 3, 1, 1)
        self.lrelu = nn.LeakyReLU(negative_slope=0.2)

    def forward(self, x):
        x1 = self.lrelu(self.conv1(x))
        x2 = self.lrelu(self.conv2(torch.cat((x, x1), 1)))
        x3 = self.lrelu(self.conv3(torch.cat((x, x1, x2), 1)))
        x4 = self.lrelu(self.conv4(torch.cat((x, x1, x2, x3), 1)))
        x5 = self.conv5(torch.cat((x, x1, x2, x3, x4), 1))
        return x5 * 0.2 + x


class RRDB(nn.Module):
    def __init__(self, num_feat, num_grow_ch=32):
        super().__init__()
        self.rdb1 = ResidualDenseBlock(num_feat, num_grow_ch)
        self.rdb2 = ResidualDenseBlock(num_feat, num_grow_ch)
        self.rdb3 = ResidualDenseBlock(num_feat, num_grow_ch)

    def forward(self, x):
        out = self.rdb1(x)
        out = self.rdb2(out)
        out = self.rdb3(out)
        return out * 0.2 + x


class PixelUnshuffle2(nn.Module):
    """F.pixel_unshuffle(x, 2) for 3 channels, as a stride-2 convolution: output channel
    c*4 + dy*2 + dx takes input channel c at (2y + dy, 2x + dx)."""

    def __init__(self, channels=3):
        super().__init__()
        w = torch.zeros(channels * 4, channels, 2, 2)
        for c in range(channels):
            for dy in range(2):
                for dx in range(2):
                    w[c * 4 + dy * 2 + dx, c, dy, dx] = 1.0
        self.register_buffer("weight", w)

    def forward(self, x):
        return F.conv2d(x, self.weight, stride=2)


class RRDBNet(nn.Module):
    """RealESRGAN_x2plus: scale 2, 64 features, 23 RRDB blocks, growth 32."""

    def __init__(self, num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32):
        super().__init__()
        self.unshuffle = PixelUnshuffle2(num_in_ch)
        self.conv_first = nn.Conv2d(num_in_ch * 4, num_feat, 3, 1, 1)
        self.body = nn.Sequential(*[RRDB(num_feat, num_grow_ch) for _ in range(num_block)])
        self.conv_body = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_up1 = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_up2 = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_hr = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_last = nn.Conv2d(num_feat, num_out_ch, 3, 1, 1)
        self.lrelu = nn.LeakyReLU(negative_slope=0.2)

    def forward(self, x):
        feat = self.conv_first(self.unshuffle(x))
        feat = feat + self.conv_body(self.body(feat))
        feat = self.lrelu(self.conv_up1(F.interpolate(feat, scale_factor=2.0, mode="nearest")))
        feat = self.lrelu(self.conv_up2(F.interpolate(feat, scale_factor=2.0, mode="nearest")))
        return self.conv_last(self.lrelu(self.conv_hr(feat)))
