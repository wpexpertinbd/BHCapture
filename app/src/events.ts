import type { EventCallback, UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

/**
 * Listen for events meant for THIS window: ones sent to it by label, plus app-wide broadcasts.
 *
 * Tauri 2's plain `listen()` also receives events sent to OTHER windows, so with two editors open,
 * File > Save or a close request for one reached both -- each saved, or closed (audit 2026-10-13).
 */
export function listen<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  return getCurrentWebviewWindow().listen<T>(event, handler);
}
