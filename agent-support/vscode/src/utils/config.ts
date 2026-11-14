import * as vscode from "vscode";
import * as os from "node:os";
import * as path from "node:path";

const GIT_AI_HOME_DIR = ".git-ai";
const INTERNAL_DIR = "internal";

export const localPaths = {
  config: () => path.join(os.homedir(), GIT_AI_HOME_DIR, "config.json"),
  distinctId: () => path.join(os.homedir(), GIT_AI_HOME_DIR, INTERNAL_DIR, "distinct_id"),
  vscodeIdeNames: () => path.join(os.homedir(), GIT_AI_HOME_DIR, "VSCODE_IDE_NAMES"),
};

export type BlameMode = 'off' | 'line' | 'all';

export class Config {
  private static getRoot(): vscode.WorkspaceConfiguration {
    return vscode.workspace.getConfiguration("gitai");
  }

  static isCheckpointLoggingEnabled(): boolean {
    return !!this.getRoot().get<boolean>("enableCheckpointLogging");
  }

  static isAiTabTrackingEnabled(): boolean {
    return !!this.getRoot().get<boolean>("experiments.aiTabTracking");
  }

  static getBlameMode(): BlameMode {
    const mode = this.getRoot().get<string>("blameMode");
    if (mode === 'off' || mode === 'line' || mode === 'all') {
      return mode;
    }
    return 'off'; // default
  }

  static async setBlameMode(mode: BlameMode): Promise<void> {
    await this.getRoot().update("blameMode", mode, vscode.ConfigurationTarget.Global);
  }
}

