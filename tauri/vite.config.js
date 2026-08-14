import { defineConfig } from 'vite';
import { viteStaticCopy } from 'vite-plugin-static-copy';
import { fileURLToPath, URL } from 'node:url';

export default defineConfig({
    clearScreen: false,
    base: './',
    root: '..',
    plugins: [
        viteStaticCopy({
            targets: [
                {
                    src: ['css_js/**', '!css_js/snipaste.png', '!index_cdn.html'],
                    dest: '.',
                }
            ],
        }),
    ],
    server: {
        port: 5173,
        strictPort: true,
        watch: {
            ignored: [
                '**/tauri/src-tauri/target/**',
                '**/tauri/src-tauri/gen/**',
                '**/.venv/**',
                '**/backend/**',
                '**/.idea/**',
                '**/.mdgo/**',
                '**/.vscode/**',
                '**/.git/**',
                '**/data/**',
                '**/index_cdn.html/**',
                '**/*.tmp',
                '**/*.TMP',
            ],
        },
    },
    envPrefix: ['VITE_', 'TAURI_'],
    build: {
        outDir: 'tauri/dist',
        target: ['es2021', 'chrome105', 'safari13'],
        minify: !process.env.TAURI_DEBUG ? 'esbuild' : false,
        sourcemap: !!process.env.TAURI_DEBUG,
        rollupOptions: {
            input: {
                main: fileURLToPath(new URL('../main.html', import.meta.url)),
            },
        },
    },
});
