import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { Record, Workspace } from '../workspace';
import {
  BLOCK_KINDS,
  appendBlocks,
  deleteBlock,
  documentText,
  findInBlocks,
  insertBlock,
  readBlocks,
  replaceBlocks,
  updateBlock,
} from './blocks';
import { addPage, newPageId, pageExists, readCatalog, removePage, type PageMeta } from './library';

/** Broadcast changed records to every connected editor. */
export type Broadcast = (records: Record[]) => void;

const runSchema = z.object({
  text: z.string(),
  bold: z.boolean().optional(),
  italic: z.boolean().optional(),
});

const blockSchema = z.object({
  kind: z.enum(BLOCK_KINDS).optional(),
  text: z.string().optional(),
  runs: z.array(runSchema).optional(),
});

/** Build an MCP server exposing the Essor document model as tools. */
export function createMcpServer(workspace: Workspace, broadcast: Broadcast): McpServer {
  const server = new McpServer({ name: 'essor', version: '0.1.0' });

  server.registerTool(
    'list_documents',
    {
      title: 'List documents',
      description:
        'List every document in the Essor library, in sidebar order. Use the returned id for the other document tools.',
      inputSchema: {},
    },
    async () => result({ documents: readCatalog(workspace) }),
  );

  server.registerTool(
    'get_document',
    {
      title: 'Get document',
      description:
        'Read one document as structured blocks. Each block has a kind (paragraph, heading1, heading2, bullet), its plain text, and runs preserving bold/italic marks.',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
      },
    },
    async ({ id }) => {
      const meta = requirePage(workspace, id);
      const blocks = readBlocks(workspace, id);
      return result({ id, title: meta.title, blocks, text: documentText(blocks) });
    },
  );

  server.registerTool(
    'search_documents',
    {
      title: 'Search documents',
      description: 'Case-insensitive text search across every document in the library.',
      inputSchema: {
        query: z.string().describe('Text to look for.'),
        limit: z
          .number()
          .int()
          .positive()
          .max(100)
          .optional()
          .describe('Maximum documents to return.'),
      },
    },
    async ({ query, limit }) => {
      const results: Array<{ id: string; title: string; matches: ReturnType<typeof findInBlocks> }> = [];
      for (const meta of readCatalog(workspace)) {
        const matches = findInBlocks(readBlocks(workspace, meta.id), query);
        if (matches.length > 0) {
          results.push({ id: meta.id, title: meta.title, matches });
        }
        if (limit !== undefined && results.length >= limit) {
          break;
        }
      }
      return result({ query, results });
    },
  );

  server.registerTool(
    'create_document',
    {
      title: 'Create document',
      description:
        'Create a new document, add it to the end of the library, and return its id. Seeds one empty paragraph when no blocks are given.',
      inputSchema: {
        title: z.string().describe('Title shown in the sidebar.'),
        blocks: z.array(blockSchema).optional().describe('Initial content.'),
      },
    },
    async ({ title, blocks }) => {
      const id = newPageId();
      const changed: Record[] = [addPage(workspace, id, title)];
      const seed = blocks !== undefined && blocks.length > 0 ? blocks : [{ kind: 'paragraph' as const }];
      changed.push(...replaceBlocks(workspace, id, seed));
      broadcast(changed);
      return result({ id, title, blockCount: seed.length });
    },
  );

  server.registerTool(
    'delete_document',
    {
      title: 'Delete document',
      description:
        'Remove a document from the library. Refuses to delete the last remaining document.',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
      },
    },
    async ({ id }) => {
      requirePage(workspace, id);
      const entries = readCatalog(workspace);
      if (entries.length <= 1) {
        throw new Error('refusing to delete the last document');
      }
      const tombstone = removePage(workspace, id);
      if (tombstone !== undefined) {
        broadcast([tombstone]);
      }
      return result({ deleted: true, id, remaining: entries.length - 1 });
    },
  );

  server.registerTool(
    'append_blocks',
    {
      title: 'Append blocks',
      description: 'Append one or more blocks to the end of a document.',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
        blocks: z.array(blockSchema).min(1).describe('Blocks to append, in order.'),
      },
    },
    async ({ id, blocks }) => {
      requirePage(workspace, id);
      const total = readBlocks(workspace, id).length + blocks.length;
      broadcast(appendBlocks(workspace, id, blocks));
      return result({ id, appended: blocks.length, total });
    },
  );

  server.registerTool(
    'replace_document',
    {
      title: 'Replace document',
      description:
        'Replace the entire body of a document with the given blocks. Refuses an empty body, so a document always keeps at least one block.',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
        blocks: z.array(blockSchema).min(1).describe('The complete new content.'),
      },
    },
    async ({ id, blocks }) => {
      requirePage(workspace, id);
      if (blocks.length === 0) {
        throw new Error('refusing to replace a document with an empty body');
      }
      broadcast(replaceBlocks(workspace, id, blocks));
      return result({ id, replaced: blocks.length });
    },
  );

  server.registerTool(
    'insert_block',
    {
      title: 'Insert block',
      description: 'Insert a single block at an index (0 inserts at the top).',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
        index: z.number().int().min(0).describe('Insertion index.'),
        block: blockSchema.describe('The block to insert.'),
      },
    },
    async ({ id, index, block }) => {
      requirePage(workspace, id);
      broadcast([insertBlock(workspace, id, index, block)]);
      return result({ id, index, inserted: true });
    },
  );

  server.registerTool(
    'update_block',
    {
      title: 'Update block',
      description:
        'Change the kind and/or content of the block at an index. Supplying text or runs replaces the content and its marks; omit both to change only the kind.',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
        index: z.number().int().min(0).describe('Block index from get_document.'),
        kind: z.enum(BLOCK_KINDS).optional(),
        text: z.string().optional().describe('Plain replacement text (no marks).'),
        runs: z.array(runSchema).optional().describe('Replacement runs with marks; wins over text.'),
      },
    },
    async ({ id, index, kind, text, runs }) => {
      requirePage(workspace, id);
      const update = updateBlock(workspace, id, index, { kind, text, runs });
      if (update === undefined) {
        throw new Error(`cannot update block at index ${index}`);
      }
      // A no-op update writes nothing, so it must not broadcast: bumping the
      // version would let it clobber a concurrent peer edit under LWW.
      if (update.changed) {
        broadcast([update.record]);
      }
      return result({ id, index, updated: update.changed });
    },
  );

  server.registerTool(
    'delete_block',
    {
      title: 'Delete block',
      description: 'Delete the block at an index. Refuses to leave the document empty.',
      inputSchema: {
        id: z.string().describe('Document id from list_documents.'),
        index: z.number().int().min(0).describe('Block index from get_document.'),
      },
    },
    async ({ id, index }) => {
      requirePage(workspace, id);
      if (readBlocks(workspace, id).length <= 1) {
        throw new Error('refusing to delete the only block');
      }
      const tombstone = deleteBlock(workspace, id, index);
      if (tombstone === undefined) {
        throw new Error(`cannot delete block at index ${index}`);
      }
      broadcast([tombstone]);
      return result({ id, index, deleted: true });
    },
  );

  return server;
}

function result(value: unknown): { content: Array<{ type: 'text'; text: string }> } {
  return { content: [{ type: 'text', text: JSON.stringify(value, null, 2) }] };
}

function requirePage(workspace: Workspace, id: string): PageMeta {
  if (!pageExists(workspace, id)) {
    throw new Error(`unknown document: ${id}. Use list_documents to see ids.`);
  }
  return readCatalog(workspace).find((entry) => entry.id === id) ?? { id, title: 'Untitled' };
}
