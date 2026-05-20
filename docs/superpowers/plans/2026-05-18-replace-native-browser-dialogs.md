# Replace Native Browser Dialogs in Admin UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace all native browser dialogs (`alert`, `confirm`, `prompt`) in `src/server/admin.html` with UIkit modal/notification equivalents.

**Architecture:** Add the UIKit JS CDN script tag so the already-written notification code starts working, then mechanically replace each `confirm()` with `await UIkit.modal.confirm()` + try/catch and the single `prompt()` with `await UIkit.modal.prompt()`. All call sites are already `async` — no function signature changes needed.

**Tech Stack:** UIKit 3 (CDN), vanilla JS, single-file HTML served by Rust (`src/server/admin.html`)

**Note on testing:** This project has no JS test framework. Each task uses browser console verification as the acceptance check instead of automated tests.

---

## File Map

| File | Changes |
|------|---------|
| `src/server/admin.html` line 26 | Add UIKit JS `<script>` tag after Tailwind |
| `src/server/admin.html` lines 2171–2183 | Replace `notify()` body |
| `src/server/admin.html` lines 2549–2554 | Replace `confirm()` in `deleteProvider()` |
| `src/server/admin.html` lines 2807–2810 | Replace `confirm()` in `deleteModel()` |
| `src/server/admin.html` lines 2802–2804 | Replace `confirm()` in `restartServer()` |
| `src/server/admin.html` lines 3836–3842 | Replace `confirm()` in `saveAndRestart()` |
| `src/server/admin.html` lines 4397–4404 | Replace `confirm()` in `deleteOAuthToken()` |
| `src/server/admin.html` lines 2151–2157 | Replace `prompt()` in `apiFetch()` |

---

### Task 1: Add UIKit JS Script Tag

**Files:**
- Modify: `src/server/admin.html:26` (after Tailwind CSS script tag)

The franken-ui CSS is already loaded (line 14). UIKit JS is missing — that's why `typeof UIkit === "undefined"` is always true and `alert()` fires every time. Adding this one script tag fixes the root cause.

- [ ] **Step 1: Add the UIKit JS script tag**

Open `src/server/admin.html`. Find line 26 which reads:
```html
        <script src="https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4"></script>
```

Add the UIKit script tag immediately after it so the block looks like:
```html
        <!-- Tailwind CSS -->
        <script src="https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4"></script>

        <!-- UIKit JS (required for notifications and modals) -->
        <script src="https://cdn.jsdelivr.net/npm/uikit@3/dist/js/uikit.min.js"></script>
```

- [ ] **Step 2: Verify in browser**

Build and open the admin UI, then open DevTools console and run:
```javascript
typeof UIkit
```
Expected output: `"object"` (not `"undefined"`)

- [ ] **Step 3: Commit**

```bash
git add src/server/admin.html
git commit -m "feat: add UIKit JS CDN for notifications and modals"
```

---

### Task 2: Replace `notify()` Body

**Files:**
- Modify: `src/server/admin.html:2171–2183`

The current `notify()` falls back to `alert()` when UIKit is not loaded (lines 2172–2175). Now that UIKit JS is present (Task 1), remove the alert fallback and add an `escapeHtml()` call — UIKit renders notification `message` as HTML, so raw strings are an XSS vector.

- [ ] **Step 1: Replace the notify() function body**

Find this exact block (lines 2171–2183):
```javascript
            function notify(message, status = "primary") {
                if (typeof UIkit === "undefined" || !UIkit.notification) {
                    // Fallback to alert if UIkit is not loaded
                    alert(message);
                    return;
                }
                UIkit.notification({
                    message: message,
                    status: status,
                    pos: "top-right",
                    timeout: 3000,
                });
            }
```

Replace it with:
```javascript
            function notify(message, status = "primary") {
                if (typeof UIkit === "undefined") {
                    console.error("[notify] UIkit not loaded:", message);
                    return;
                }
                UIkit.notification({
                    message: escapeHtml(message),
                    status: status,
                    pos: "top-right",
                    timeout: 3000,
                });
            }
```

Key changes:
- Dead `alert()` branch replaced with `console.error` guard
- `escapeHtml(message)` prevents HTML injection in toast content

- [ ] **Step 2: Verify in browser**

Build and open the admin UI. In DevTools console run:
```javascript
notify("Hello <b>world</b>", "success")
```
Expected: Toast appears top-right showing literal text `Hello <b>world</b>` (tags not rendered as HTML). Color is green (success).

Also run:
```javascript
notifyError("Something went wrong")
```
Expected: Red toast appears top-right.

- [ ] **Step 3: Commit**

```bash
git add src/server/admin.html
git commit -m "fix: replace alert() fallback in notify() with console.error guard and escapeHtml"
```

---

### Task 3: Replace 5x `confirm()` Calls

**Files:**
- Modify: `src/server/admin.html` at lines 2549, 2807, 3802, 3836, 4397

Pattern — replace each `if (!confirm(...)) return;` with an async UIKit modal confirm. The cancel path is handled by the Promise rejection (UIKit rejects when user clicks Cancel or presses Escape).

All 5 functions are already `async` — no signature changes needed.

**Pattern to apply at each site:**
```javascript
// BEFORE
if (!confirm("Message")) return;

// AFTER
try {
    await UIkit.modal.confirm("Message");
} catch (e) {
    return;
}
```

- [ ] **Step 1: Replace confirm() in deleteProvider() — line 2549**

Find (lines 2549–2554):
```javascript
            async function deleteProvider(index) {
                if (
                    !confirm("Are you sure you want to delete this provider?")
                ) {
                    return;
                }
```

Replace with:
```javascript
            async function deleteProvider(index) {
                try {
                    await UIkit.modal.confirm("Are you sure you want to delete this provider?");
                } catch (e) {
                    return;
                }
```

- [ ] **Step 2: Replace confirm() in deleteModel() — line 2807**

Find (lines 2807–2810):
```javascript
            async function deleteModel(index) {
                if (!confirm("Are you sure you want to delete this model?")) {
                    return;
                }
```

Replace with:
```javascript
            async function deleteModel(index) {
                try {
                    await UIkit.modal.confirm("Are you sure you want to delete this model?");
                } catch (e) {
                    return;
                }
```

- [ ] **Step 3: Replace confirm() in restartServer() — line 3802**

Find (lines 3802–3804):
```javascript
            async function restartServer() {
                if (!confirm("Are you sure you want to restart the server?"))
                    return;
```

Replace with:
```javascript
            async function restartServer() {
                try {
                    await UIkit.modal.confirm("Are you sure you want to restart the server?");
                } catch (e) {
                    return;
                }
```

- [ ] **Step 4: Replace confirm() in saveAndRestart() — line 3836**

Find (lines 3836–3842):
```javascript
            async function saveAndRestart() {
                if (
                    !confirm(
                        "Save settings and Are you sure you want to restart the server?",
                    )
                )
                    return;
```

Replace with (also fix the grammar bug in the message):
```javascript
            async function saveAndRestart() {
                try {
                    await UIkit.modal.confirm("Save all settings and restart. Are you sure?");
                } catch (e) {
                    return;
                }
```

- [ ] **Step 5: Replace confirm() in deleteOAuthToken() — line 4397**

Find (lines 4397–4404):
```javascript
            async function deleteOAuthToken(providerId) {
                if (
                    !confirm(
                        `Are you sure you want to delete the OAuth token for "${providerId}"?`,
                    )
                ) {
                    return;
                }
```

Replace with (also add escapeHtml since providerId goes into dialog text):
```javascript
            async function deleteOAuthToken(providerId) {
                try {
                    await UIkit.modal.confirm(`Are you sure you want to delete the OAuth token for "${escapeHtml(providerId)}"?`);
                } catch (e) {
                    return;
                }
```

- [ ] **Step 6: Verify in browser**

Build and open the admin UI. Test each path:

1. Click delete on a provider → UIKit modal appears with "Are you sure you want to delete this provider?" → click Cancel → nothing happens. Click OK → provider is deleted.
2. Click delete on a model → UIKit modal appears → Escape key → nothing happens. Click OK → model is deleted.
3. Click Restart Server → UIKit modal → Cancel → nothing happens. OK → server restarts.
4. Click Save & Restart → UIKit modal with "Save all settings and restart. Are you sure?" → Cancel → nothing. OK → saves and restarts.
5. (If OAuth tokens visible) Delete OAuth token → UIKit modal → Cancel → nothing. OK → token deleted.

- [ ] **Step 7: Commit**

```bash
git add src/server/admin.html
git commit -m "feat: replace 5x confirm() with UIkit.modal.confirm() in admin UI"
```

---

### Task 4: Replace `prompt()` in `apiFetch()`

**Files:**
- Modify: `src/server/admin.html:2151–2157`

`UIkit.modal.prompt()` returns a Promise that resolves with the entered string, or `null` on cancel — matching `window.prompt` behavior. The existing `if (key)` guard handles the null-on-cancel case correctly. `apiFetch` is already `async`.

- [ ] **Step 1: Replace prompt() in apiFetch()**

Find (lines 2151–2157):
```javascript
    const key = prompt("API key required. Enter your server API key:");
    if (key) {
      sessionStorage.setItem("ccm_api_key", key);
      headers["X-Api-Key"] = key;
      return fetch(url, { ...options, headers });
    }
```

Replace with:
```javascript
    const key = await UIkit.modal.prompt("API key required. Enter your server API key:", "");
    if (key) {
      sessionStorage.setItem("ccm_api_key", key);
      headers["X-Api-Key"] = key;
      return fetch(url, { ...options, headers });
    }
```

- [ ] **Step 2: Verify in browser**

To trigger the 401 path: open the admin UI with an incorrect API key set (or clear `sessionStorage` and configure the server to require one). Make any API call.

Expected flows:
- UIKit prompt modal appears with "API key required. Enter your server API key:"
- Cancel → modal closes, the original 401 response is returned (no key stored)
- Enter a valid key and confirm → key stored in `sessionStorage`, request retried with the key

- [ ] **Step 3: Commit**

```bash
git add src/server/admin.html
git commit -m "feat: replace prompt() with UIkit.modal.prompt() in apiFetch()"
```

---

## Final Smoke Test

After all four tasks, do a full end-to-end check:

- [ ] Open DevTools → Console tab. Filter for `alert` / `confirm` / `prompt` — none should fire.
- [ ] Run in console: `typeof window.alert` — still `"function"` (we didn't remove it from the browser), but verify it's never called by adding a breakpoint on `window.alert` and performing all actions above.
- [ ] Save config → toast notification appears top-right (not an alert box).
- [ ] Delete provider → UIKit modal, not a browser confirm dialog.
- [ ] Restart server → UIKit modal.
- [ ] Save & Restart → UIKit modal with corrected grammar.
- [ ] Trigger 401 → UIKit prompt modal (not a browser prompt dialog).

All native dialog boxes should be gone. All notifications should appear as dismissable toasts in the top-right corner.
