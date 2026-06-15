document.addEventListener('DOMContentLoaded', () => {
    const apiBaseInput = document.getElementById('apiBase');
    const saveBtn = document.getElementById('saveBtn');
    const statusEl = document.getElementById('status');

    chrome.storage.sync.get(['apiBase'], (result) => {
        if (result.apiBase) {
            apiBaseInput.value = result.apiBase;
        }
    });

    saveBtn.addEventListener('click', () => {
        const apiBase = apiBaseInput.value.trim();

        if (!apiBase) {
            showStatus('请输入 API 服务器地址', 'error');
            return;
        }

        try {
            new URL(apiBase);
        } catch {
            showStatus('请输入有效的 URL', 'error');
            return;
        }

        chrome.storage.sync.set({ apiBase }, () => {
            if (chrome.runtime.lastError) {
                showStatus('保存失败: ' + chrome.runtime.lastError.message, 'error');
            } else {
                showStatus('配置已保存', 'success');
            }
        });
    });

    function showStatus(message, type) {
        statusEl.textContent = message;
        statusEl.className = 'status ' + type;
        setTimeout(() => {
            statusEl.className = 'status';
        }, 2000);
    }
});