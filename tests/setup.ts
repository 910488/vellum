/**
 * jsdom 25 沒有實作 <dialog>：showModal / close 根本不存在，open 也不會動。
 * 這是環境的缺口，不是元件的問題 —— 所以補在測試環境，不補進 ui.tsx。
 * 一旦應用程式碼開始 `typeof node.showModal === "function"` 這樣試探，
 * 生產環境就永遠走不到被測過的那一條路。
 *
 * 只補行為，不補樣式：::backdrop、焦點鎖、惰性背景交給真的瀏覽器。
 */
if (typeof HTMLDialogElement !== "undefined" && !HTMLDialogElement.prototype.showModal) {
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) {
    this.open = true;
  };
  HTMLDialogElement.prototype.show = function show(this: HTMLDialogElement) {
    this.open = true;
  };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement, returnValue?: string) {
    this.open = false;
    if (returnValue !== undefined) this.returnValue = returnValue;
    this.dispatchEvent(new Event("close"));
  };
}
