---
id: raw-photography
scope: system
name: RAW 照片解析
description: 当用户要求解析 RAW 照片（.arw/.cr2/.nef/.dng/.orf 等）的拍摄参数与图像信息时触发。
priority: 40
tools: [raw-photography]
triggers: [RAW, 照片, arw, cr2, nef, dng, orf, 相机, 镜头, 曝光, 拍摄参数, 光圈, 快门, ISO]
enabled: true
version: 2
created_at: 1754200000000
updated_at: 1754200000000
---

## 职责边界
本 Skill 用于：
1. 解析 RAW 文件元数据。
2. 分析摄影参数合理性。
3. 判断可能拍摄场景。
4. 给出拍摄优化建议。
5. 给出 Lightroom / Camera Raw 后期建议。

禁止：

- 不主动扫描用户目录。
- 不遍历未知照片。
- 不修改原始 RAW 文件。
- 不伪造不存在的数据。

所有解析均在本地完成。


## 工作流程
1. 调用 raw-photography 解析 RAW 文件元数据信息
2. 解析信息，进行曝光诊断、场景自动分类、参数纠错、问题诊断
3. 给出优化方案、后期修图参数建议、下次拍摄最优参数建议
4. 进行打分

## 注意事项
- `path` 是知识库根目录下的相对路径，如 `note/photo/IMG_0001.arw`；不支持知识库外路径。
- 如果 raw-photography 解析失败，返回失败原因，不要伪造参数，不要重复调用。
- 若解析结果为空（无任何可用标签），返回「解析成功但无可用元数据」。
