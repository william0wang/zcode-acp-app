// Haptic feedback (requirements 1.10): navigator.vibrate on the Android
// WebView (VIBRATE permission in the manifest), a silent no-op everywhere
// else. Single short taps only — UI feedback, not notification signaling
// (OS notifications are out of scope, ADR-0010).
export function haptic(ms = 8): void {
  try {
    navigator.vibrate?.(ms);
  } catch {
    // Unsupported platform — fine.
  }
}
