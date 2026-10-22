# Darkslide acknowledgements

Darkslide's code is licensed under the Apache License 2.0. The models below are credited with their licences. Some of them were trained on data licensed for non-commercial research only; Darkslide's licence doesn't override those terms, and complying with them is up to you. Those models aren't bundled in public builds: they are downloaded on first use from the source named here. The licences of the Rust crates Darkslide links follow these credits.

## AI Denoise model: NAFNet

AI Denoise runs NAFNet-SIDD-width32:

- **Paper:** Liangyu Chen, Xiaojie Chu, Xiangyu Zhang and Jian Sun, *Simple Baselines for Image Restoration*, ECCV 2022.
- **Code and weights:** https://github.com/megvii-research/NAFNet. The architecture is at commit 2b4af71. The weights come from a mirror at huggingface.co/mikestealth/nafnet-models, commit cd3a91d, file `NAFNet-SIDD-width32.pth`, SHA-256 89c70e808d1783b6c07911306e106aaf0d4f7f3da8c61078b99ff7f8929a26f4.
- **Training data:** SIDD (Smartphone Image Denoising Dataset), released under the MIT License (https://abdokamel.github.io/sidd/).
- **Bundled model:** Darkslide converts it to Core ML float16 when the app is built (`scripts/denoise-model/`).

Licence of NAFNet and its weights:

MIT License

Copyright (c) 2022 megvii-model

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.



BasicSR
Copyright 2018-2020 BasicSR Authors

                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION

   1. Definitions.

      "License" shall mean the terms and conditions for use, reproduction,
      and distribution as defined by Sections 1 through 9 of this document.

      "Licensor" shall mean the copyright owner or entity authorized by
      the copyright owner that is granting the License.

      "Legal Entity" shall mean the union of the acting entity and all
      other entities that control, are controlled by, or are under common
      control with that entity. For the purposes of this definition,
      "control" means (i) the power, direct or indirect, to cause the
      direction or management of such entity, whether by contract or
      otherwise, or (ii) ownership of fifty percent (50%) or more of the
      outstanding shares, or (iii) beneficial ownership of such entity.

      "You" (or "Your") shall mean an individual or Legal Entity
      exercising permissions granted by this License.

      "Source" form shall mean the preferred form for making modifications,
      including but not limited to software source code, documentation
      source, and configuration files.

      "Object" form shall mean any form resulting from mechanical
      transformation or translation of a Source form, including but
      not limited to compiled object code, generated documentation,
      and conversions to other media types.

      "Work" shall mean the work of authorship, whether in Source or
      Object form, made available under the License, as indicated by a
      copyright notice that is included in or attached to the work
      (an example is provided in the Appendix below).

      "Derivative Works" shall mean any work, whether in Source or Object
      form, that is based on (or derived from) the Work and for which the
      editorial revisions, annotations, elaborations, or other modifications
      represent, as a whole, an original work of authorship. For the purposes
      of this License, Derivative Works shall not include works that remain
      separable from, or merely link (or bind by name) to the interfaces of,
      the Work and Derivative Works thereof.

      "Contribution" shall mean any work of authorship, including
      the original version of the Work and any modifications or additions
      to that Work or Derivative Works thereof, that is intentionally
      submitted to Licensor for inclusion in the Work by the copyright owner
      or by an individual or Legal Entity authorized to submit on behalf of
      the copyright owner. For the purposes of this definition, "submitted"
      means any form of electronic, verbal, or written communication sent
      to the Licensor or its representatives, including but not limited to
      communication on electronic mailing lists, source code control systems,
      and issue tracking systems that are managed by, or on behalf of, the
      Licensor for the purpose of discussing and improving the Work, but
      excluding communication that is conspicuously marked or otherwise
      designated in writing by the copyright owner as "Not a Contribution."

      "Contributor" shall mean Licensor and any individual or Legal Entity
      on behalf of whom a Contribution has been received by Licensor and
      subsequently incorporated within the Work.

   2. Grant of Copyright License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      copyright license to reproduce, prepare Derivative Works of,
      publicly display, publicly perform, sublicense, and distribute the
      Work and such Derivative Works in Source or Object form.

   3. Grant of Patent License. Subject to the terms and conditions of
      this License, each Contributor hereby grants to You a perpetual,
      worldwide, non-exclusive, no-charge, royalty-free, irrevocable
      (except as stated in this section) patent license to make, have made,
      use, offer to sell, sell, import, and otherwise transfer the Work,
      where such license applies only to those patent claims licensable
      by such Contributor that are necessarily infringed by their
      Contribution(s) alone or by combination of their Contribution(s)
      with the Work to which such Contribution(s) was submitted. If You
      institute patent litigation against any entity (including a
      cross-claim or counterclaim in a lawsuit) alleging that the Work
      or a Contribution incorporated within the Work constitutes direct
      or contributory patent infringement, then any patent licenses
      granted to You under this License for that Work shall terminate
      as of the date such litigation is filed.

   4. Redistribution. You may reproduce and distribute copies of the
      Work or Derivative Works thereof in any medium, with or without
      modifications, and in Source or Object form, provided that You
      meet the following conditions:

      (a) You must give any other recipients of the Work or
          Derivative Works a copy of this License; and

      (b) You must cause any modified files to carry prominent notices
          stating that You changed the files; and

      (c) You must retain, in the Source form of any Derivative Works
          that You distribute, all copyright, patent, trademark, and
          attribution notices from the Source form of the Work,
          excluding those notices that do not pertain to any part of
          the Derivative Works; and

      (d) If the Work includes a "NOTICE" text file as part of its
          distribution, then any Derivative Works that You distribute must
          include a readable copy of the attribution notices contained
          within such NOTICE file, excluding those notices that do not
          pertain to any part of the Derivative Works, in at least one
          of the following places: within a NOTICE text file distributed
          as part of the Derivative Works; within the Source form or
          documentation, if provided along with the Derivative Works; or,
          within a display generated by the Derivative Works, if and
          wherever such third-party notices normally appear. The contents
          of the NOTICE file are for informational purposes only and
          do not modify the License. You may add Your own attribution
          notices within Derivative Works that You distribute, alongside
          or as an addendum to the NOTICE text from the Work, provided
          that such additional attribution notices cannot be construed
          as modifying the License.

      You may add Your own copyright statement to Your modifications and
      may provide additional or different license terms and conditions
      for use, reproduction, or distribution of Your modifications, or
      for any such Derivative Works as a whole, provided Your use,
      reproduction, and distribution of the Work otherwise complies with
      the conditions stated in this License.

   5. Submission of Contributions. Unless You explicitly state otherwise,
      any Contribution intentionally submitted for inclusion in the Work
      by You to the Licensor shall be under the terms and conditions of
      this License, without any additional terms or conditions.
      Notwithstanding the above, nothing herein shall supersede or modify
      the terms of any separate license agreement you may have executed
      with Licensor regarding such Contributions.

   6. Trademarks. This License does not grant permission to use the trade
      names, trademarks, service marks, or product names of the Licensor,
      except as required for reasonable and customary use in describing the
      origin of the Work and reproducing the content of the NOTICE file.

   7. Disclaimer of Warranty. Unless required by applicable law or
      agreed to in writing, Licensor provides the Work (and each
      Contributor provides its Contributions) on an "AS IS" BASIS,
      WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
      implied, including, without limitation, any warranties or conditions
      of TITLE, NON-INFRINGEMENT, MERCHANTABILITY, or FITNESS FOR A
      PARTICULAR PURPOSE. You are solely responsible for determining the
      appropriateness of using or redistributing the Work and assume any
      risks associated with Your exercise of permissions under this License.

   8. Limitation of Liability. In no event and under no legal theory,
      whether in tort (including negligence), contract, or otherwise,
      unless required by applicable law (such as deliberate and grossly
      negligent acts) or agreed to in writing, shall any Contributor be
      liable to You for damages, including any direct, indirect, special,
      incidental, or consequential damages of any character arising as a
      result of this License or out of the use or inability to use the
      Work (including but not limited to damages for loss of goodwill,
      work stoppage, computer failure or malfunction, or any and all
      other commercial damages or losses), even if such Contributor
      has been advised of the possibility of such damages.

   9. Accepting Warranty or Additional Liability. While redistributing
      the Work or Derivative Works thereof, You may choose to offer,
      and charge a fee for, acceptance of support, warranty, indemnity,
      or other liability obligations and/or rights consistent with this
      License. However, in accepting such obligations, You may act only
      on Your own behalf and on Your sole responsibility, not on behalf
      of any other Contributor, and only if You agree to indemnify,
      defend, and hold each Contributor harmless for any liability
      incurred by, or claims asserted against, such Contributor by reason
      of your accepting any such warranty or additional liability.

   END OF TERMS AND CONDITIONS

   APPENDIX: How to apply the Apache License to your work.

      To apply the Apache License to your work, attach the following
      boilerplate notice, with the fields enclosed by brackets "[]"
      replaced with your own identifying information. (Don't include
      the brackets!)  The text should be enclosed in the appropriate
      comment syntax for the file format. We also recommend that a
      file or class name and description of purpose be included on the
      same "printed page" as the copyright notice for easier
      identification within third-party archives.

   Copyright 2018-2020 BasicSR Authors

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.

## AI Upscale model: Real-ESRGAN

AI Upscale runs RealESRGAN_x2plus:

- **Paper:** Xintao Wang, Liangbin Xie, Chao Dong and Ying Shan, *Real-ESRGAN: Training Real-World Blind Super-Resolution with Pure Synthetic Data*, ICCV Workshops 2021.
- **Code and weights:** https://github.com/xinntao/Real-ESRGAN (BSD 3-Clause License, below). The weights are the release file `RealESRGAN_x2plus.pth` (v0.2.1), SHA-256 49fafd45f8fd7aa8d31ab2a22d14d91b536c34494a5cfe31eb5d89c2fa266abb. The network is RRDBNet from BasicSR (Apache License 2.0, above).
- **Training data:** DIV2K, Flickr2K and OutdoorSceneTraining, licensed for non-commercial research only. Darkslide's licence doesn't override those terms; complying with them is up to you. Public builds download this model on first use (Photo ▸ Enhance ▸ Download AI Upscale Model…).
- **The model Darkslide uses:** the weights converted to Core ML float16 by `scripts/upscale-model/`; the weights and model spec are checked by SHA-256 when downloaded. Builds made with `DS_BUNDLE_UPSCALE=1` (personal builds) include it instead of downloading it.

Licence of Real-ESRGAN and its weights:

BSD 3-Clause License

Copyright (c) 2021, Xintao Wang
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived from
   this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

## Remove: LaMa

Remove fills with big-lama, downloaded the first time something is removed:

- **Paper:** Roman Suvorov et al., *Resolution-robust Large Mask Inpainting with Fourier Convolutions*, WACV 2022.
- **Code and weights:** https://github.com/advimman/lama (Apache License 2.0). Core ML conversion by john-rocky at huggingface.co/mlboydaisuke/LaMa-CoreML, commit 5ed76e3, checked by SHA-256.
- **Training data:** big-lama was trained on Places365, whose terms allow non-commercial research and educational use only. Darkslide's licence doesn't override those terms; complying with them is up to you. This model is downloaded from huggingface.co/mlboydaisuke/LaMa-CoreML on first use.

## People masks: BiSeNet face parsing

Face, eyes, teeth, hair and skin masks use a BiSeNet face-parsing model, downloaded the first time a people sub-part is masked:

- **Code and weights:** https://github.com/zllrunning/face-parsing.PyTorch (MIT License). Core ML conversion by john-rocky at huggingface.co/mlboydaisuke/Face-Parsing-CoreML, commit 6af21c2, checked by SHA-256.
- **Training data:** CelebAMask-HQ, licensed for non-commercial research and educational use only. Darkslide's licence doesn't override those terms; complying with them is up to you. This model is downloaded from huggingface.co/mlboydaisuke/Face-Parsing-CoreML on first use.

## Lens blur: Depth Anything V2 Small

The lens blur's depth map uses Depth Anything V2 Small, downloaded the first time the depth blur is used (about 50 MB):

- **Paper:** Lihe Yang, Bingyi Kang, Zilong Huang, Zhen Zhao, Xiaogang Xu, Jiashi Feng and Hengshuang Zhao, *Depth Anything V2*, 2024 (arXiv:2406.09414).
- **Code and weights:** https://github.com/DepthAnything/Depth-Anything-V2. The Small model's weights are licensed under the Apache License 2.0 (the Base and Large models are not, and aren't used).
- **Core ML conversion:** Apple, huggingface.co/apple/coreml-depth-anything-v2-small (Apache License 2.0), `DepthAnythingV2SmallF16.mlpackage` at commit cfef6f6, checked by SHA-256.

## Sky masks: DETR ResNet-50 semantic segmentation

AI Sky masks use DETR ResNet-50 semantic segmentation, downloaded the first time a sky mask is made:

- **Core ML conversion:** Apple, huggingface.co/apple/coreml-detr-semantic-segmentation (Apache License 2.0), `DETRResnet50SemanticSegmentationF16.mlpackage` at commit 7c771f8, checked by SHA-256.

## Search: MobileCLIP-S2

Natural-language search uses Apple's MobileCLIP-S2 image and text encoders, downloaded the first time search is used:

- **Core ML conversion:** Apple, huggingface.co/apple/coreml-mobileclip at commit 3e0a7bf, checked by SHA-256. The weights are under Apple's sample-code licence for MobileCLIP (`apple-ascl`, LICENSE_weights_data in https://github.com/apple/ml-mobileclip), which allows use, reproduction and redistribution with its notice kept.
- **Tokenizer:** CLIP's BPE merges (`merges.txt`) from huggingface.co/openai/clip-vit-base-patch32 (MIT License, © 2021 OpenAI).

## Soft proofing and output profiles: Little CMS

Soft proofing and exports to an ICC profile use Little CMS 2 (https://www.littlecms.com), linked statically through the Rust crates `lcms2` and `lcms2-sys` (https://github.com/kornelski/rust-lcms2).

Licence of Little CMS:

MIT License

Copyright (c) 2023 Marti Maria Saguer

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject
to the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

Licence of `lcms2` and `lcms2-sys`: MIT, Copyright (c) Kornel Lesiński, with the same permission notice and warranty disclaimer as above.
