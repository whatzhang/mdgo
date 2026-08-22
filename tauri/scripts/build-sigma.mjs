/**
 * ===== Sigma.js v3 本地打包脚本（tauri/scripts/build-sigma.mjs） =====
 *
 * 【为什么需要打包】Sigma v3 是纯 ESM 库，其 dist 产物内部 `import 'graphology'` 等
 * 裸模块名在浏览器/iframe 中无法解析（无 bundler、无 import map）。直接拷贝 npm 产物
 * 到 css_js/cdn/sigma/ 会报 "Failed to resolve module specifier"。
 * 本脚本用 esbuild 将 @sigma/core + graphology + forceAtlas2 打成一个**自包含单文件 ESM**
 * （内部依赖全部内联），浏览器可直接 `import()`。
 *
 * 【运行】`npm run build:sigma`（或 `node scripts/build-sigma.mjs`）
 * 【产物】css_js/cdn/sigma/sigma.bundle.js（随 vite staticCopy 的 css_js/** 分发到 dist）
 * 【约定】graph-app.js 的 SIGMA_PATHS 已指向该 bundle（named exports 对齐）：
 *   import { Sigma, Graph, forceAtlas2 } from bundle
 */
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';

const root = path.resolve(fileURLToPath(new URL('..', import.meta.url))); // tauri/
const outDir = path.join(root, '..', 'css_js', 'cdn', 'sigma');
const entryFile = path.join(outDir, 'sigma-entry.js');
const outFile = path.join(outDir, 'sigma.bundle.js');

// 1. 生成临时入口（re-export 三个包；forceAtlas2 为 default 导出函数）
const entry = `export { Sigma } from 'sigma';
export { default as Graph } from 'graphology';
import forceAtlas2 from 'graphology-layout-forceatlas2';
export { forceAtlas2 };
`;
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(entryFile, entry);

// 2. esbuild 打包为自包含 ESM
try {
  await build({
    entryPoints: [entryFile],
    bundle: true,
    format: 'esm',
    platform: 'browser',
    target: ['es2020', 'chrome105'],
    minify: true,
    sourcemap: false,
    outfile: outFile,
    logLevel: 'info',
    // 解析 node_modules（入口在 css_js/ 下，需显式指向 tauri/node_modules）
    nodePaths: [path.join(root, 'node_modules')],
    // Sigma 有少量依赖（如 eventemitter3）会被自动内联；显式 external 只留浏览器内置
    external: [],
  });
  console.log(`[build:sigma] 完成: ${path.relative(root, outFile)}`);
} finally {
  // 清理临时入口
  fs.rmSync(entryFile, { force: true });
}
