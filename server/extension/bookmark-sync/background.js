const DEFAULT_API_BASE = 'http://localhost:8090';

let apiBase = DEFAULT_API_BASE;

chrome.storage.sync.get(['apiBase'], (result) => {
    if (chrome.runtime.lastError) {
        console.error('[Bookmark Sync] 读取配置失败:', chrome.runtime.lastError.message);
    } else if (result.apiBase) {
        apiBase = result.apiBase;
    }
});

chrome.storage.onChanged.addListener((changes, areaName) => {
    if (areaName === 'sync' && changes.apiBase) {
        apiBase = changes.apiBase.newValue || DEFAULT_API_BASE;
    }
});

const TIMEOUT_MS = 5 * 60 * 1000;

async function postJSON(path, body) {
    try {
        const controller = new AbortController();
        const timeoutId = setTimeout(() => {
            controller.abort();
        }, TIMEOUT_MS);

        const response = await fetch(`${apiBase}${path}`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
            signal: controller.signal
        });

        clearTimeout(timeoutId);

        if (!response.ok) {
            console.error(`[Bookmark Sync] POST ${path} HTTP 错误: ${response.status}`);
            showAlarm(`网络请求失败: ${response.status}`);
            return;
        }

        const resp = await response.json();
        if (resp.code !== 200) {
            console.error(`[Bookmark Sync] POST ${path} 业务错误: ${resp.message || '未知错误'}`);
            showAlarm(resp.message || '同步失败');
        }
    } catch (err) {
        if (err.name === 'AbortError') {
            console.error(`[Bookmark Sync] POST ${path} 请求超时`);
            showAlarm('请求超时');
        } else {
            console.error(`[Bookmark Sync] 网络错误: ${err.message}`);
            showAlarm(`网络错误: ${err.message}`);
        }
    }
}

function showAlarm(message) {
    chrome.action.setBadgeText({ text: '!' });
    chrome.action.setBadgeBackgroundColor({ color: '#cf222e' });

    chrome.notifications.create('bookmark-sync-alarm-' + Date.now(), {
        type: 'basic',
        iconUrl: 'icons/icon48.png',
        title: '书签同步失败',
        message: message,
        priority: 2
    });

    setTimeout(() => {
        chrome.action.setBadgeText({ text: '' });
    }, 5000);
}

chrome.bookmarks.onCreated.addListener((id, bookmark) => {
    if (bookmark.url) {
        console.log('[Bookmark Sync] 新增书签:', bookmark.url);
        postJSON('/api/bm/bookmarks/add', { url: bookmark.url });
    }
});

chrome.bookmarks.onRemoved.addListener((id, removeInfo) => {
    const bookmark = removeInfo.node;
    if (bookmark.url) {
        console.log('[Bookmark Sync] 删除书签:', bookmark.url);
        postJSON('/api/bm/bookmarks/delete_by_url', { url: bookmark.url });
    }
});