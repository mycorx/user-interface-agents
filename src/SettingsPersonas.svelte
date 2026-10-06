<script lang="ts">
  import { convertFileSrc, invoke } from '@tauri-apps/api/core';
  import { open } from '@tauri-apps/plugin-dialog';

  // Mirrors `uia_app::personas::Persona` — duplicated across the IPC
  // boundary like every other settings tab's types.
  type Persona = {
    id: string;
    label: string;
    archetype: string;
    manner: string;
    use_when: string;
    boundaries: string;
    extra: string;
    name_override: string;
    avatar_ext: string | null;
    enabled: boolean;
  };
  type Book = { active: string; allow_agent_switch: boolean; personas: Persona[] };
  type Res = { book: Book; notices: string[]; avatar_paths: Record<string, string> };

  const MAX_PERSONAS = 6;
  const REQUIRED = ['label', 'archetype', 'manner', 'use_when'] as const;
  // Mirrors `uia_app::personas::DEFAULT_PERSONA_ID`. Duplicated across the IPC
  // boundary like the types above; the backend is still the enforcer.
  const DEFAULT_PERSONA_ID = 'default';

  let { onclose, onchange }: { onclose: () => void; onchange: () => void } = $props();

  let book = $state<Book | null>(null);
  let notices = $state<string[]>([]);
  // id -> absolute path, supplied by the backend. The frontend never builds
  // one of these: `settings::persona_avatar_path_for` owns the id-to-filename
  // rule, which is what keeps a persona id off the filesystem in any shape
  // the app did not choose. Only personas whose image is really on disk
  // appear here, so a stale `avatar_ext` falls through to the initial.
  let avatarPaths = $state<Record<string, string>>({});
  // Why the last avatar pick was refused, if it was.
  let avatarError = $state<string | null>(null);
  // `null` is the list; a persona id is the editor standing in front of it.
  // Same shape as SettingsMcp's `wizard`, for the same reason: at 600px a
  // master-detail split leaves neither pane enough width for `manner`.
  let editing = $state<string | null>(null);

  const editingPersona = $derived(book?.personas.find((p) => p.id === editing) ?? null);

  function missing(p: Persona): string[] {
    return REQUIRED.filter((f) => !p[f].trim());
  }

  async function load() {
    const res = await invoke<Res>('get_personas');
    book = res.book;
    notices = res.notices;
    avatarPaths = res.avatar_paths;
  }

  // Every mutation saves immediately and takes the backend's corrected book
  // back. There is no separate Save button: a settings frame the user closes
  // mid-edit must not lose what they typed.
  async function save() {
    if (!book) return;
    const res = await invoke<Res>('save_personas', { book });
    book = res.book;
    notices = res.notices;
    avatarPaths = res.avatar_paths;
    onchange();
  }

  function add() {
    if (!book || book.personas.length >= MAX_PERSONAS) return;
    // An empty id asks the backend to derive one from the label; the
    // frontend never invents an id, because the id is what becomes a
    // filename.
    book.personas.push({
      id: '',
      label: 'New persona',
      archetype: '',
      manner: '',
      use_when: '',
      boundaries: '',
      extra: '',
      name_override: '',
      avatar_ext: null,
      enabled: false,
    });
    void save();
  }

  function duplicate(source: Persona) {
    if (!book || book.personas.length >= MAX_PERSONAS) return;
    book.personas.push({ ...source, id: '', label: `${source.label} copy`, enabled: false });
    void save();
  }

  function remove(id: string) {
    if (!book || book.personas.length === 1) return;
    book.personas = book.personas.filter((p) => p.id !== id);
    if (editing === id) editing = null;
    void save();
  }

  // Mirrors `settings::AVATAR_EXTENSIONS`. The backend is still the enforcer —
  // this only decides what the file dialog offers to show.
  const AVATAR_EXTENSIONS = ['png', 'jpg', 'jpeg', 'gif', 'webp', 'avif'];

  async function pickAvatar(id: string) {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: 'Images', extensions: AVATAR_EXTENSIONS }],
    });
    if (typeof selected !== 'string') return;
    // Shown, not swallowed: the backend refuses an image it cannot decode and
    // one too large to shrink, and both of those are things the user can act
    // on. Without this the promise rejects into nothing and the avatar simply
    // does not change, which is indistinguishable from the bug where the
    // extension was never saved.
    avatarError = null;
    try {
      await invoke<string>('set_persona_avatar', { id, sourcePath: selected });
    } catch (e) {
      avatarError = String(e);
      return;
    }
    await load();
  }

  $effect(() => {
    void load();
  });
</script>

<!-- One face, two sizes. A persona with no image falls back to a monochrome
     tile carrying the first letter of its name, so every row reads the same
     shape whether or not a picture has been chosen — an empty slot next to a
     filled one is what made the avatar look broken rather than unset. The
     letter follows `label`, so it updates as the name is typed. -->
{#snippet face(persona: Persona, size: 'sm' | 'lg')}
  {#if avatarPaths[persona.id]}
    <img
      class="face {size}"
      src={convertFileSrc(avatarPaths[persona.id])}
      alt=""
      aria-hidden="true"
    />
  {:else}
    <span class="face {size} initial" aria-hidden="true">
      {(persona.label.trim()[0] ?? '?').toUpperCase()}
    </span>
  {/if}
{/snippet}

{#if book}
  {#each notices as notice}
    <p class="field-error">{notice}</p>
  {/each}

  {#if editingPersona}
    <!-- Editor step. Four visible fields; the rest behind "More", so the
         common case is four inputs rather than seven. Placeholders
         demonstrate rather than describe — "executive secretary" teaches
         what an archetype is, "the role this persona plays" does not. -->
    <!-- Face and name together at the top, the shape every profile editor
         uses. The avatar was previously the last field inside the collapsed
         "More", which put the one persona field with a visible consequence
         outside this frame — the face in the HUD — four levels from it. -->
    <div class="identity-row">
      {@render face(editingPersona, 'lg')}
      <div class="field identity-name">
        <label for="p-label">Name</label>
        <input id="p-label" type="text" bind:value={editingPersona.label} placeholder="Work" />
      </div>
      <button type="button" onclick={() => pickAvatar(editingPersona.id)}>
        {avatarPaths[editingPersona.id] ? 'Change…' : 'Add photo…'}
      </button>
    </div>
    {#if avatarError}
      <p class="field-error">{avatarError}</p>
    {/if}

    <div class="field">
      <label for="p-archetype">Archetype</label>
      <input
        id="p-archetype"
        type="text"
        bind:value={editingPersona.archetype}
        placeholder="executive secretary"
      />
    </div>

    <div class="field">
      <label for="p-manner">How it speaks</label>
      <textarea
        id="p-manner"
        rows="3"
        bind:value={editingPersona.manner}
        placeholder="Crisp and businesslike. Confirms details before acting."
      ></textarea>
    </div>

    <div class="field">
      <label for="p-usewhen">Use when</label>
      <textarea
        id="p-usewhen"
        rows="2"
        bind:value={editingPersona.use_when}
        placeholder="During working hours, for scheduling and correspondence."
      ></textarea>
    </div>

    <details class="field more">
      <summary>More</summary>
      <div class="field">
        <label for="p-boundaries">Never</label>
        <textarea
          id="p-boundaries"
          rows="2"
          bind:value={editingPersona.boundaries}
          placeholder="Never send anything on my behalf without asking."
        ></textarea>
      </div>

      <div class="field">
        <label for="p-extra">Extra instructions</label>
        <textarea id="p-extra" rows="3" bind:value={editingPersona.extra}></textarea>
      </div>

      <div class="field">
        <label for="p-name">Assistant name</label>
        <input
          id="p-name"
          type="text"
          bind:value={editingPersona.name_override}
          placeholder="Leave blank to keep the assistant's usual name."
        />
      </div>
    </details>

    <div class="save-row">
      <!-- The plan's own Done handler is just `save` — which persists the
           edit but never clears `editing`, leaving the editor stuck open
           with no other control that does. Saving and returning to the list
           is what "Done" has to mean here. -->
      <button
        type="button"
        onclick={async () => {
          await save();
          editing = null;
        }}
      >
        Done
      </button>
    </div>
  {:else}
    <!-- List step. -->
    {#each book.personas as persona (persona.id)}
      <div class="persona-row">
        <!-- Disabled while required fields are blank, for a stronger reason
             than tidiness: `active` is the one field that decides the system
             prompt, and an unfinished persona composes to "You are ." with a
             blank manner. `LoadOutcome::from_book` enforces the same rule on
             load, so a hand-edited sidecar cannot get past this either. -->
        <input
          type="radio"
          name="active"
          checked={book.active === persona.id}
          disabled={missing(persona).length > 0}
          title={missing(persona).length > 0
            ? 'Finish this persona before making it the active one.'
            : undefined}
          onchange={() => {
            if (!book) return;
            book.active = persona.id;
            void save();
          }}
          aria-label={`Make ${persona.label} the active persona`}
        />
        <!-- The thumbnail opens the editor rather than the file picker: it
             sits beside the name and does the same thing the name does, so
             two adjacent controls that looked alike but behaved differently
             would be the surprise. Changing the picture is one click further,
             on the header the editor now opens with. -->
        <button
          type="button"
          class="persona-face"
          onclick={() => (editing = persona.id)}
          tabindex="-1"
          aria-hidden="true"
        >
          {@render face(persona, 'sm')}
        </button>
        <button type="button" class="persona-name" onclick={() => (editing = persona.id)}>
          {persona.label || 'Untitled'}
        </button>
        <!-- A persona missing a required field saves fine and simply cannot
             be enabled. Losing half-written work to a validation gate is
             worse than holding a disabled draft. -->
        <input
          type="checkbox"
          bind:checked={persona.enabled}
          disabled={missing(persona).length > 0}
          title={missing(persona).length > 0
            ? `Needs ${missing(persona).join(', ')} before it can be offered to the assistant.`
            : undefined}
          onchange={save}
          aria-label={`Offer ${persona.label} to the assistant`}
        />
        {#if missing(persona).length > 0}
          <span class="hint">needs {missing(persona).join(', ')}</span>
        {/if}
        <button type="button" onclick={() => duplicate(persona)}>Duplicate</button>
        <!-- Disabled rather than hidden on the default persona, for the same
             reason the six-persona cap shows a notice instead of a dead
             button: a control that vanishes reads as a bug, and the row goes
             ragged against its siblings. `default` is the identity every
             fallback path lands on -- `from_book` restores it when the list
             empties and prefers it when `active` stops resolving -- so
             deleting it is survivable but never useful. -->
        <button
          type="button"
          class="danger"
          onclick={() => remove(persona.id)}
          disabled={book.personas.length === 1 || persona.id === DEFAULT_PERSONA_ID}
          title={persona.id === DEFAULT_PERSONA_ID
            ? 'Default is the identity the assistant falls back to. It cannot be deleted, but you can rename it or change what it says.'
            : undefined}
        >
          Delete
        </button>
      </div>
    {/each}

    <div class="field">
      <label class="checkbox-label">
        <input type="checkbox" bind:checked={book.allow_agent_switch} onchange={save} />
        Let the assistant suggest switching persona
      </label>
      <p class="hint">
        It will ask before switching, and say so afterwards. That rule is a strong
        instruction rather than a guarantee &mdash; if it ever switches unasked,
        say &ldquo;switch back&rdquo;.
      </p>
    </div>

    <div class="save-row">
      <button type="button" onclick={add} disabled={book.personas.length >= MAX_PERSONAS}>
        Add persona
      </button>
      {#if book.personas.length >= MAX_PERSONAS}
        <!-- Visible reason, not a silently dead button: the cap exists to keep
             the assistant's tool list short, which is not guessable from a
             greyed-out control. -->
        <span class="hint"
          >{MAX_PERSONAS} is the maximum, so the assistant's list of personas
          stays short enough for it to choose between them reliably.</span
        >
      {/if}
    </div>

    <div class="save-row">
      <button type="button" onclick={onclose}>Done</button>
    </div>
  {/if}
{/if}

<style>
  .field {
    margin-bottom: 16px;
  }

  .field:last-child {
    margin-bottom: 0;
  }

  .field label {
    display: block;
    font-weight: 500;
    margin-bottom: 6px;
  }

  .field input[type='text'],
  .field textarea {
    width: 100%;
    box-sizing: border-box;
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 10px;
    resize: vertical;
  }

  .field.more {
    margin-top: 4px;
  }

  .field.more summary {
    cursor: pointer;
    font-weight: 500;
    margin-bottom: 10px;
  }

  .checkbox-label {
    display: flex !important;
    align-items: center;
    gap: 8px;
    font-weight: 500;
    cursor: pointer;
  }

  .checkbox-label input {
    accent-color: var(--accent, #6ea8fe);
  }

  .hint {
    margin: 6px 0 0;
    font-size: 12px;
    color: #8890a0;
  }

  .field-error {
    margin: 0 0 10px;
    font-size: 12px;
    color: #d9534f;
  }

  .persona-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 0;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  }

  .persona-row input[type='radio'] {
    accent-color: var(--accent, #6ea8fe);
    flex-shrink: 0;
  }

  .persona-row input[type='checkbox'] {
    accent-color: var(--accent, #6ea8fe);
    flex-shrink: 0;
  }

  /* A disabled tickbox at the browser default reads as "unticked" rather than
     "unavailable" -- the two look nearly identical, which is how a user comes
     to think the control is simply not working. Dim it hard enough that the
     difference is the first thing seen, and take the cursor away too, since
     the tooltip explaining why is on hover. */
  .persona-row input:disabled {
    opacity: 0.35;
    cursor: not-allowed;
  }

  /* Deliberately monochrome. A generated colour per persona would be the
     obvious alternative, but the placeholder sits next to real photographs
     in the same list, and a coloured tile competes with them for attention
     while saying nothing true about the persona. Grey reads as "no picture
     yet" rather than as a choice someone made. */
  .face {
    border-radius: 50%;
    flex-shrink: 0;
    object-fit: cover;
    display: grid;
    place-items: center;
    background: rgba(255, 255, 255, 0.08);
    color: #aab2c0;
    font-weight: 600;
    line-height: 1;
    user-select: none;
  }

  .face.initial {
    border: 1px solid rgba(255, 255, 255, 0.12);
  }

  .face.sm {
    width: 26px;
    height: 26px;
    font-size: 12px;
  }

  .face.lg {
    width: 64px;
    height: 64px;
    font-size: 24px;
  }

  /* The row's thumbnail is a button only so the whole tile is a hit target
     for opening the editor; it carries none of the button chrome. */
  .persona-face {
    background: none;
    border: none;
    padding: 0;
    display: flex;
    flex-shrink: 0;
    cursor: pointer;
  }

  .persona-face:hover .face {
    filter: brightness(1.15);
  }

  .identity-row {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 16px;
  }

  /* Carries `.field` so the label and input keep the styling every other
     field in this frame has; the row supplies the spacing instead, so the
     field's own bottom margin would double it. The name is also the part
     that gives, so the face keeps its circle and the button keeps its label
     at any width the card is given. */
  .identity-name {
    flex: 1;
    min-width: 0;
    margin-bottom: 0;
  }

  .persona-name {
    flex: 1;
    min-width: 0;
    text-align: left;
    font: inherit;
    font-weight: 500;
    color: inherit;
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .persona-name:hover {
    color: var(--accent, #6ea8fe);
  }

  .persona-row .hint {
    margin: 0;
    flex-shrink: 0;
  }

  button {
    font: inherit;
    color: inherit;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px 12px;
    cursor: pointer;
  }

  button:not(:disabled):hover {
    background: var(--accent, #6ea8fe);
    color: #0c0e12;
    border-color: var(--accent, #6ea8fe);
  }

  button.danger:not(:disabled):hover {
    background: #d0424a;
    border-color: #d0424a;
    color: #fff;
  }

  button:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .save-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 16px;
  }
</style>
