/**
 * mermaid 工具
 * @param {*} code mermaid 代码
 * @param {*} type check 检验语法错误， render 渲染
 * @returns 
 */
window.mermaidTool = function(code, type) {
    try {
        const { svg } =  mermaid.render('svg-' + Date.now(), code);
        if (type === 'check') {
            return { success: true, msg: '语法正确', data: {} };
        }
        return { success: true, msg: '渲染成功', data: {svg: svg} };
    } catch (error) {
        const errMsg = error?.message || error?.str || '未知语法错误';
        console.error('Mermaid 渲染失败:', error);
        return { success: false, msg: errMsg, data: {} };
    }
}
