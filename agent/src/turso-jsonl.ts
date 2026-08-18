import { posix as path } from "node:path";

import {
  err,
  FileError,
  ok,
  type FileErrorCode,
  type FileInfo,
  type FileKind,
  type Result,
} from "@earendil-works/pi-agent-core";
import {
  createClient,
  type Client,
} from "@tursodatabase/serverless/compat";

type EntryKind = "file" | "directory";

interface EntryRow {
  path: string;
  kind: EntryKind;
  content: string | null;
  mtime_ms: number;
}

const FS_SCHEMA = `
  CREATE TABLE IF NOT EXISTS wt_pi_fs_entries (
    path TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    content TEXT,
    mtime_ms INTEGER NOT NULL
  );
`;

export class TursoJsonlFileSystem {
  readonly cwd: string;

  constructor(
    private readonly db: Client,
    cwd = "/worktable",
  ) {
    this.cwd = cwd;
  }

  async migrate(): Promise<void> {
    await this.db.execute(FS_SCHEMA);
    await this.createDirectory("/", true);
    await this.createDirectory(this.cwd, true);
  }

  asPiFileSystem() {
    return {
      cwd: this.cwd,
      absolutePath: this.absolutePath.bind(this),
      joinPath: this.joinPath.bind(this),
      readTextFile: this.readTextFile.bind(this),
      readTextLines: this.readTextLines.bind(this),
      writeFile: this.writeFile.bind(this),
      appendFile: this.appendFile.bind(this),
      renameFile: this.renameFile.bind(this),
      fileInfo: this.fileInfo.bind(this),
      listDir: this.listDir.bind(this),
      exists: this.exists.bind(this),
      createDir: this.createDir.bind(this),
      remove: this.remove.bind(this),
    };
  }

  private async load(pathname: string): Promise<EntryRow | undefined> {
    const result = await this.db.execute(
      "SELECT path, kind, content, mtime_ms FROM wt_pi_fs_entries WHERE path = ?",
      [pathname],
    );
    return (result.rows[0] as unknown as EntryRow | undefined) ?? undefined;
  }

  private async allEntries(): Promise<EntryRow[]> {
    const result = await this.db.execute(
      "SELECT path, kind, content, mtime_ms FROM wt_pi_fs_entries",
    );
    return result.rows as unknown as EntryRow[];
  }

  private normalize(input: string): string {
    const absolute = input.startsWith("/")
      ? input
      : path.join(this.cwd, input);
    const normalized = path.normalize(absolute);
    return normalized === "." ? "/" : normalized;
  }

  private toFileInfo(row: EntryRow): FileInfo {
    const name = row.path === "/" ? "/" : path.basename(row.path);
    const size =
      row.kind === "file"
        ? Buffer.byteLength(row.content ?? "", "utf8")
        : 0;

    return {
      name,
      path: row.path,
      kind: row.kind as FileKind,
      size,
      mtimeMs: Number(row.mtime_ms),
    };
  }

  private error(
    code: FileErrorCode,
    message: string,
    pathname: string,
    cause?: unknown,
  ): FileError {
    return new FileError(
      code,
      message,
      pathname,
      cause instanceof Error ? cause : undefined,
    );
  }

  private async fallible<T>(
    pathname: string,
    operation: () => Promise<T>,
  ): Promise<Result<T, FileError>> {
    try {
      return ok(await operation());
    } catch (cause) {
      if (cause instanceof FileError) {
        return err(cause);
      }

      return err(
        this.error(
          "unknown",
          cause instanceof Error ? cause.message : String(cause),
          pathname,
          cause,
        ),
      );
    }
  }

  async absolutePath(input: string): Promise<Result<string, FileError>> {
    return this.fallible(input, async () => this.normalize(input));
  }

  async joinPath(parts: string[]): Promise<Result<string, FileError>> {
    return this.fallible(parts.join("/"), async () =>
      this.normalize(path.join(...parts)),
    );
  }

  async readTextFile(pathname: string): Promise<Result<string, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => {
      const row = await this.load(normalized);
      if (!row) {
        throw this.error("not_found", `File does not exist: ${normalized}`, normalized);
      }
      if (row.kind !== "file") {
        throw this.error("is_directory", `Path is a directory: ${normalized}`, normalized);
      }

      return row.content ?? "";
    });
  }

  async readTextLines(
    pathname: string,
    options?: { maxLines?: number },
  ): Promise<Result<string[], FileError>> {
    const result = await this.readTextFile(pathname);
    if (!result.ok) {
      return result;
    }

    const lines = result.value.split(/\r?\n/);
    return ok(
      options?.maxLines === undefined
        ? lines
        : lines.slice(0, options.maxLines),
    );
  }

  async writeFile(
    pathname: string,
    content: string | Uint8Array,
  ): Promise<Result<void, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => {
      const existing = await this.load(normalized);
      if (existing?.kind === "directory") {
        throw this.error("is_directory", `Path is a directory: ${normalized}`, normalized);
      }

      const text =
        typeof content === "string"
          ? content
          : Buffer.from(content).toString("utf8");
      await this.db.execute(
        `INSERT INTO wt_pi_fs_entries (path, kind, content, mtime_ms)
         VALUES (?, 'file', ?, ?)
         ON CONFLICT(path) DO UPDATE SET
           kind = 'file',
           content = excluded.content,
           mtime_ms = excluded.mtime_ms`,
        [normalized, text, Date.now()],
      );
    });
  }

  async appendFile(
    pathname: string,
    content: string | Uint8Array,
  ): Promise<Result<void, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => {
      const existing = await this.load(normalized);
      if (existing?.kind === "directory") {
        throw this.error("is_directory", `Path is a directory: ${normalized}`, normalized);
      }

      const text =
        typeof content === "string"
          ? content
          : Buffer.from(content).toString("utf8");
      await this.db.execute(
        `INSERT INTO wt_pi_fs_entries (path, kind, content, mtime_ms)
         VALUES (?, 'file', ?, ?)
         ON CONFLICT(path) DO UPDATE SET
           kind = 'file',
           content = COALESCE(wt_pi_fs_entries.content, '') || excluded.content,
           mtime_ms = excluded.mtime_ms`,
        [normalized, text, Date.now()],
      );
    });
  }

  async renameFile(
    sourcePath: string,
    destinationPath: string,
  ): Promise<Result<void, FileError>> {
    const source = this.normalize(sourcePath);
    const destination = this.normalize(destinationPath);

    return this.fallible(source, async () => {
      const transaction = await this.db.transaction("write");
      try {
        const sourceResult = await transaction.execute({
          sql: "SELECT path, kind, content, mtime_ms FROM wt_pi_fs_entries WHERE path = ?",
          args: [source],
        });
        const row = sourceResult.rows[0] as unknown as EntryRow | undefined;
        if (!row) {
          throw this.error("not_found", `File does not exist: ${source}`, source);
        }
        if (row.kind !== "file") {
          throw this.error("is_directory", `Path is a directory: ${source}`, source);
        }

        await transaction.execute({
          sql: "DELETE FROM wt_pi_fs_entries WHERE path = ?",
          args: [destination],
        });
        await transaction.execute({
          sql: `INSERT INTO wt_pi_fs_entries (path, kind, content, mtime_ms)
                VALUES (?, 'file', ?, ?)
                ON CONFLICT(path) DO UPDATE SET
                  kind = 'file',
                  content = excluded.content,
                  mtime_ms = excluded.mtime_ms`,
          args: [destination, row.content ?? "", Date.now()],
        });
        await transaction.execute({
          sql: "DELETE FROM wt_pi_fs_entries WHERE path = ?",
          args: [source],
        });
        await transaction.commit();
      } catch (cause) {
        await transaction.rollback().catch(() => undefined);
        throw cause;
      }
    });
  }

  async fileInfo(pathname: string): Promise<Result<FileInfo, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => {
      const row = await this.load(normalized);
      if (!row) {
        throw this.error("not_found", `Path does not exist: ${normalized}`, normalized);
      }

      return this.toFileInfo(row);
    });
  }

  async listDir(pathname: string): Promise<Result<FileInfo[], FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => {
      const directory = await this.load(normalized);
      if (!directory) {
        throw this.error("not_found", `Directory does not exist: ${normalized}`, normalized);
      }
      if (directory.kind !== "directory") {
        throw this.error("not_directory", `Path is not a directory: ${normalized}`, normalized);
      }

      const prefix = normalized === "/" ? "/" : `${normalized}/`;
      const children = (await this.allEntries()).filter((entry) => {
        if (!entry.path.startsWith(prefix) || entry.path === normalized) {
          return false;
        }

        return !entry.path.slice(prefix.length).includes("/");
      });

      return children.map((entry) => this.toFileInfo(entry));
    });
  }

  async exists(pathname: string): Promise<Result<boolean, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => (await this.load(normalized)) !== undefined);
  }

  async createDir(
    pathname: string,
    options?: { recursive?: boolean },
  ): Promise<Result<void, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, () =>
      this.createDirectory(normalized, options?.recursive ?? true),
    );
  }

  async remove(
    pathname: string,
    options?: { recursive?: boolean; force?: boolean },
  ): Promise<Result<void, FileError>> {
    const normalized = this.normalize(pathname);
    return this.fallible(normalized, async () => {
      const row = await this.load(normalized);
      if (!row) {
        if (options?.force) {
          return;
        }
        throw this.error("not_found", `Path does not exist: ${normalized}`, normalized);
      }

      if (row.kind === "file") {
        await this.db.execute(
          "DELETE FROM wt_pi_fs_entries WHERE path = ?",
          [normalized],
        );
        return;
      }

      const children = (await this.allEntries()).filter(
        (entry) =>
          entry.path === normalized ||
          entry.path.startsWith(`${normalized}/`),
      );
      if (children.length > 1 && !options?.recursive) {
        throw this.error("unknown", `Directory is not empty: ${normalized}`, normalized);
      }

      for (const child of children) {
        await this.db.execute(
          "DELETE FROM wt_pi_fs_entries WHERE path = ?",
          [child.path],
        );
      }
    });
  }

  async cleanup(): Promise<void> {
    // The remote client is shared with the worker and is closed by worker shutdown.
  }

  private async createDirectory(
    pathname: string,
    recursive: boolean,
  ): Promise<void> {
    const normalized = this.normalize(pathname);
    const parts = normalized.split("/").filter(Boolean);
    const directories = normalized === "/"
      ? ["/"]
      : recursive
        ? parts.map((_, index) => `/${parts.slice(0, index + 1).join("/")}`)
      : [normalized];

    for (const directory of directories) {
      const existing = await this.load(directory);
      if (existing?.kind === "file") {
        throw this.error("invalid", `A file exists at ${directory}`, directory);
      }

      await this.db.execute(
        `INSERT INTO wt_pi_fs_entries (path, kind, content, mtime_ms)
         VALUES (?, 'directory', NULL, ?)
         ON CONFLICT(path) DO NOTHING`,
        [directory, Date.now()],
      );
    }
  }
}

export function createTursoJsonlFileSystem(
  url: string,
  authToken: string,
): TursoJsonlFileSystem {
  const db = createClient({ url, authToken });
  return new TursoJsonlFileSystem(db);
}

export async function migrateTursoPiFiles(
  url: string,
  authToken: string,
): Promise<TursoJsonlFileSystem> {
  const filesystem = createTursoJsonlFileSystem(url, authToken);
  await filesystem.migrate();
  return filesystem;
}
