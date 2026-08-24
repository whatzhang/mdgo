// TipTap bundle 入口：导出块编辑所需 API 到 window.TipTap（v3 命名导出）
import { Editor } from '@tiptap/core';
import StarterKit from '@tiptap/starter-kit';
import { TaskList, TaskItem } from '@tiptap/extension-list';
import { Table, TableRow, TableHeader, TableCell } from '@tiptap/extension-table';
import { Image } from '@tiptap/extension-image';
import { DragHandle } from '@tiptap/extension-drag-handle';

export {
    Editor,
    StarterKit,
    TaskList,
    TaskItem,
    Table,
    TableRow,
    TableHeader,
    TableCell,
    Image,
    DragHandle
};
