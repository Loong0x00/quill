# T-0616 Squircle SDF (圆角 → 超椭圆 Apple iOS 风)

**Phase**: 7+ (polish, 跟 Apple iOS / macOS 圆角视觉对齐)
**Assigned**: writer-T0616
**Status**: claimed
**Budget**: tokenBudget=80k (单一 WGSL fn 替换 + 2 处调用 + 1 测试文件)
**Dependencies**: T-0610 part 2 (surface corner mask shader) / T-0615 (per-element rounded shader)
**Priority**: P3 (polish, 视觉差异化)

## Goal

把 quill 的 4 处圆角从普通圆弧 (circular arc) 升级到 squircle (超椭圆,
L^n 范数 n≈5). 视觉跟 Apple iOS / macOS 圆角"continuous curvature" 对齐.

**4 处圆角**:
1. 窗口 4 角 (T-0610 part 2 surface corner mask)
2. Active tab body / + 按钮 box / hover inactive (T-0615 rounded pipeline)
3. Min/Max/Close 圆形 button (T-0615, n=5 在 radius=半宽时退化为 squircle 圆,
   但视觉跟 n=2 圆已无区别 → 保持 n=2 即可, 不动)
4. Tab close × hover 红圆 (同 #3, 不动)

## Bug / Pain

普通圆弧 (圆形 SDF `length(corner_dist) - radius`) 曲率从直边的 0 在接合
处**跳变**到 1/r (G1 continuity). 用户视觉读出"被切出来贴上去"的机械感.
Apple iOS 全系用 squircle (L^n 范数, n=5), 曲率从直边 0 平滑过渡到角中央
最大值再回到 0 (G2 continuity). 视觉差异: squircle 看着像水滴塑形, 圆弧
看着像机械加工.

参 user 主对话 (2026-04-26): "apple的圆角为啥特别好" + "反正我们改没成本".

## Scope

### In

#### A. WGSL 加 squircle_sdf fn

src/wl/render.rs 内, 加可复用 helper:

```wgsl
// L^n 范数 SDF (squircle / superellipse).
// p: 距离角中心的 vec2 (绝对值已应用), radius: 角半径, n: 曲率指数 (5.0 = Apple iOS).
// 返回: 距 squircle 边界的 signed distance (负=内, 正=外).
fn squircle_sdf(p: vec2<f32>, radius: f32, n: f32) -> f32 {
    let ap = abs(p);
    return pow(pow(ap.x, n) + pow(ap.y, n), 1.0 / n) - radius;
}
```

加 const `SQUIRCLE_EXPONENT: f32 = 5.0` (Apple iOS 实测值).

#### B. surface corner mask shader 替换

T-0610 part 2 的 surface corner shader (大约 src/wl/render.rs `CORNER_WGSL`
fn 内) 把圆形 SDF 换成 squircle_sdf. 4 角 dist 计算改:
```wgsl
// 旧 (圆弧):
//   let d = length(corner_dist) - radius;
// 新 (squircle):
let d = squircle_sdf(corner_dist, radius, SQUIRCLE_EXPONENT);
```

#### C. T-0615 rounded pipeline 同步替换

T-0615 的 `ROUNDED_WGSL` fragment shader 内, per-element corner mask 路径
(elem_radius > 0 fast-path) 同样把 length() 换成 squircle_sdf.

#### D. 测试

新建 tests/squircle_sdf_e2e.rs:
- PNG verify n=2 (圆弧 baseline) vs n=5 (squircle) 4 角差异
- 抓 32×32 patch 对角对比, FFT 或 RGBA diff 显示曲率分布差异
- thread_local override SQUIRCLE_EXPONENT 让测试可切 n=2 / n=5
- 单测 cosmic-text 字形渲染 + cell pipeline 不受影响 (n=0 走 surface mask
  fast-path 等价不变)

src/wl/render.rs lib 单测:
- squircle_sdf 数学性质 (p=(0,0) → -radius, p=(radius, 0) → 0 等)
- n=2 退化为圆 (squircle_sdf(p, r, 2.0) ≈ length(p) - r ± 1e-5)

### Out

- **不动**: cell pipeline / glyph pipeline (它们不走 corner mask, 无影响)
- **不动**: Min/Max/Close 圆形 button (radius = 半宽, squircle 与 circle 视觉等价, 跳过)
- **不动**: tab close × 红圆 (同上)
- **不动**: hit_test (squircle hit test 数学复杂, 视觉 squircle vs hit_test
  圆形差异在角处 ~5% 像素, 用户感知不到, 不值)
- **不动**: ghostty / mutter / xdg-shell 协议层 (不归我们管)

## Acceptance

- [ ] 4 门 release 全绿
- [ ] WGSL squircle_sdf fn 加入 + 可复用
- [ ] surface corner shader 圆角 → squircle (PNG verify)
- [ ] T-0615 rounded pipeline 圆角 → squircle (PNG verify)
- [ ] tests/squircle_sdf_e2e.rs 加 n=2 vs n=5 对比
- [ ] 总测试 ≥460 (455 + 5 squircle 测试)
- [ ] 三源 PNG verify (writer + Lead + reviewer 都 Read)
- [ ] **手测**: cargo run --release, 视觉 4 角更"水滴"感, 跟 Apple iOS 圆角神似
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-squircle/CLAUDE.md
2. /home/user/quill-impl-squircle/docs/conventions.md
3. /home/user/quill-impl-squircle/docs/invariants.md (重点 INV-002)
4. /home/user/quill-impl-squircle/tasks/T-0616-squircle-sdf.md (派单)
5. /home/user/quill-impl-squircle/src/wl/render.rs:
   - CORNER_WGSL (T-0610 part 2 surface corner shader, ~700 行附近)
   - ROUNDED_WGSL (T-0615 per-element rounded shader, ~800 行附近)
6. /home/user/quill-impl-squircle/tests/ghostty_tab_polish_e2e.rs (T-0615 PNG 测试参考)

## 重点 review 提示

1. **n 选值**: 5.0 是 Apple iOS 实测 + Mike Swanson 反编译 app icon mask
   分析得出 (实际 Apple 用 5 段三次贝塞尔近似 squircle, 不是纯 superellipse,
   但 n=5 是最接近的纯 superellipse 值). 不需要也不应该手工调到 4.5 / 5.5,
   除非视觉测试发现明显不对.
2. **n=0 fast-path**: T-0615 ROUNDED_WGSL 已有 `if (in.elem_radius > 0.0)`
   跳过 element mask 的 fast-path. 这个不能动, 否则非圆角 quad 会走 squircle
   fragment shader, 性能下降.
3. **pow() 性能**: WGSL `pow(x, y) = exp2(y * log2(x))`, 比 `length() = sqrt(dot(p,p))`
   略贵 (大约 2-3x). 但只 corner 边缘像素走这条路径 (~ <1% 帧 pixel),
   实测帧率应无差.
4. **alpha smoothstep**: 现有圆弧 mask 用 smoothstep 做 anti-aliasing, squircle
   走同样 smoothstep 即可 (SDF 接口一致). 不需要重写 AA.
5. **n=2 退化为圆 unit test**: 数学保险, 防 squircle_sdf 写错.
