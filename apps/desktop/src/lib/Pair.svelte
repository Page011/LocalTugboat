<script lang="ts">
  import { Check, CircleAlert, QrCode, Smartphone } from "@lucide/svelte";
  import { app, beginPairing, cancelPairing, confirmPairing, dismissPairing } from "./app.svelte";
  import { ago, lastSeenText } from "./format";
  import { groupCode } from "./pairing";

  const p = $derived(app.pairing);
  const left = $derived(p.t === "qr" ? Math.max(0, Math.ceil((p.start.expires_at - app.now) / 1000)) : 0);
  const clock = $derived(`${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`);
</script>

<section class="screen" aria-labelledby="pair-h">
  <div class="screen-head"><h1 id="pair-h">Link to phone</h1></div>

  {#if p.t === "qr"}
    <div class="qr-layout">
      <div class="qr" role="img" aria-label="Pairing QR code">{@html p.start.svg}</div>
      <div class="side">
        <h2>Scan with Tugboat on your phone</h2>
        <p aria-live="off"><strong>Expires in {clock}</strong></p>
        <div><button class="btn" onclick={cancelPairing}>Cancel</button></div>
        <details>
          <summary>Pairing text</summary>
          <textarea class="mono" readonly rows="4" aria-label="Pairing text" value={p.start.payload}></textarea>
        </details>
      </div>
    </div>
  {:else if p.t === "code"}
    <div class="empty" role="alert">
      <h2>Phone shows this code?</h2>
      <p class="code">{groupCode(p.code)}</p>
      <p>{p.peerName}. Link only if the phone shows the same code.</p>
      <div class="row">
        <button class="btn primary" onclick={() => confirmPairing(true)}><Check size={16} aria-hidden="true" /> Link</button>
        <button class="btn" onclick={() => confirmPairing(false)}>Cancel</button>
      </div>
    </div>
  {:else if p.t === "done"}
    <div class="empty" role="status">
      <span class="ico ok"><Check size={32} aria-hidden="true" /></span>
      <h2>Linked to {p.peerName}</h2>
      <button class="btn primary" onclick={dismissPairing}>Done</button>
    </div>
  {:else if p.t === "cancelled"}
    <div class="empty" role="alert">
      <span class="ico danger"><CircleAlert size={32} aria-hidden="true" /></span>
      <h2>{p.message}</h2>
      <div class="row">
        <button class="btn primary" onclick={beginPairing}><QrCode size={16} aria-hidden="true" /> Try again</button>
        <button class="btn" onclick={dismissPairing}>Close</button>
      </div>
    </div>
  {:else if app.paired}
    <div class="card paired">
      <Smartphone size={28} aria-hidden="true" />
      <div class="grow">
        <h2>{app.paired.name}</h2>
        <p class="muted">Linked {ago(app.paired.paired_at, app.now)} - {lastSeenText(app.paired, app.sync, app.now).toLowerCase()}</p>
      </div>
      <button class="btn" onclick={beginPairing}><QrCode size={16} aria-hidden="true" /> Link a different phone</button>
    </div>
    <p class="muted">Logs out of {app.paired.name}</p>
  {:else}
    <div class="empty">
      <h2>Link your phone</h2>
      <ol>
        <li>Open Tugboat on the phone.</li>
        <li>Scan the code shown here.</li>
        <li>Check both screens show the same 6 digits.</li>
      </ol>
      <button class="btn primary" onclick={beginPairing}><QrCode size={16} aria-hidden="true" /> Show QR code</button>
    </div>
  {/if}
</section>

<style>
  .qr-layout {
    display: flex;
    flex-wrap: wrap;
    gap: 24px;
    align-items: flex-start;
  }
  /* The code is always black on white, whatever the theme: scanners need that contrast. */
  .qr {
    width: 264px;
    height: 264px;
    padding: 12px;
    background: #ffffff;
    border: 1px solid var(--border-strong);
    border-radius: 8px;
  }
  .qr :global(svg) {
    width: 100%;
    height: 100%;
    display: block;
  }
  .side {
    display: flex;
    flex-direction: column;
    gap: 10px;
    max-width: 360px;
  }
  textarea {
    width: 100%;
    margin-top: 6px;
    padding: 6px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--surface);
    resize: none;
  }
  .code {
    font-size: 44px;
    font-weight: 600;
    letter-spacing: 0.08em;
    color: var(--fg);
  }
  .row {
    display: flex;
    gap: 8px;
  }
  ol {
    margin: 0;
    padding-left: 20px;
    text-align: left;
  }
  .paired {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-wrap: wrap;
  }
  .grow {
    flex: 1;
    min-width: 200px;
  }
</style>
