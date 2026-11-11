import { execFile } from "child_process";
import { accessSync, constants, statSync } from "node:fs";
import * as os from "os";
import * as path from "node:path";
import * as vscode from "vscode";

const binaryName = "dcc-ai-tracking";
let resolvedPath: string | null = null;
let processBinaryPath: string | undefined;
let resolvePromise: Promise<string | null> | null = null;
let extensionMode: vscode.ExtensionMode | null = null;

/**
 * Call once at activation to pass in the extension context's mode.
 */
export function initBinaryResolver(mode: vscode.ExtensionMode): void {
  extensionMode = mode;
}

/**
 * Use a login shell in development, where the extension host can have a
 * stripped PATH.
 *
 * The result is cached after the first successful resolution.
 */
export function resolveGitAiBinary(): Promise<string | null> {
  if (extensionMode !== vscode.ExtensionMode.Development) {
    return Promise.resolve(null);
  }

  if (resolvedPath) {
    return Promise.resolve(resolvedPath);
  }
  if (resolvePromise) {
    return resolvePromise;
  }

  resolvePromise = new Promise((resolve) => {
    const platform = os.platform();

    if (platform === "win32") {
      execFile("where.exe", [binaryName], (err, stdout) => {
        if (err || !stdout.trim()) {
          console.log("[git-ai] Could not resolve git-ai binary via 'where'");
          resolve(null);
        } else {
          // `where` can return multiple lines; take the first
          resolvedPath = stdout.trim().split(/\r?\n/)[0];
          console.log("[git-ai] Resolved binary path:", resolvedPath);
          resolve(resolvedPath);
        }
      });
    } else {
      // macOS/Linux: spawn a login shell so the user's profile is sourced
      const shell = process.env.SHELL || "/bin/bash";
      execFile(shell, ["-ilc", `command -v ${binaryName}`], { timeout: 5000 }, (err, stdout) => {
        if (err || !stdout.trim()) {
          console.log("[git-ai] Could not resolve git-ai binary via login shell");
          resolve(null);
        } else {
          resolvedPath = stdout.trim();
          console.log("[git-ai] Resolved binary path:", resolvedPath);
          resolve(resolvedPath);
        }
      });
    }
  });

  return resolvePromise;
}

function findProcessBinary(): string | undefined {
  const suffix = os.platform() === "win32" ? ".exe" : "";
  const directories = [
    path.join(os.homedir(), ".dcc-ai-tracking", "bin"),
    ...(process.env.PATH ?? "").split(path.delimiter).filter(path.isAbsolute),
  ];
  for (const directory of directories) {
    const candidate = path.join(directory, binaryName + suffix);
    try {
      if (statSync(candidate).isFile()) {
        accessSync(candidate, constants.X_OK);
        return candidate;
      }
    } catch {
      // Try the next installation or PATH entry.
    }
  }
}

export function getGitAiBinary(): string {
  return resolvedPath ?? (processBinaryPath ??= findProcessBinary()) ?? binaryName;
}
