const { app, BrowserWindow, globalShortcut, ipcMain, session } = require('electron')
const { execSync, spawn } = require('child_process')
const path = require('path')
const fs = require('fs')
const os = require('os')

// ── Config ────────────────────────────────────────────────────────────────────
const WINDOW_WIDTH = 480
const WINDOW_HEIGHT = 56
const AGENT_TIMEOUT_MS = 120_000
const OLLAMA_MODEL = process.env.VOX_MODEL || 'gemma3:1b-it-qat'
const REGISTRY_PATH = path.join(os.homedir(), '.vox', 'projects.json')

let activeProjectPath = process.env.VOX_PROJECT || process.cwd()

// ── Project registry ──────────────────────────────────────────────────────────
function loadRegistry() {
  try { return JSON.parse(fs.readFileSync(REGISTRY_PATH, 'utf8')) } catch { return {} }
}

function saveRegistry(reg) {
  fs.mkdirSync(path.dirname(REGISTRY_PATH), { recursive: true })
  fs.writeFileSync(REGISTRY_PATH, JSON.stringify(reg, null, 2))
}

// Bootstrap registry with the active project on first run
function ensureRegistry() {
  if (!fs.existsSync(REGISTRY_PATH)) {
    const name = path.basename(activeProjectPath)
    saveRegistry({ [name]: activeProjectPath })
    console.log(`[vox] created ~/.vox/projects.json with "${name}"`)
  }
}

// ── CLI path detection ────────────────────────────────────────────────────────
let claudePath
try {
  claudePath = execSync('which claude', { shell: true }).toString().trim()
  console.log('[vox] claude:', claudePath)
} catch {
  console.error('[vox] claude CLI not found — agents will fail')
}

let whisperPath
try {
  // Check PATH first, then common pip --user install locations
  const candidates = [
    execSync('which whisper 2>/dev/null || true', { shell: true }).toString().trim(),
    `${os.homedir()}/Library/Python/3.9/bin/whisper`,
    `${os.homedir()}/Library/Python/3.11/bin/whisper`,
    `${os.homedir()}/.local/bin/whisper`,
  ]
  whisperPath = candidates.find(p => p && fs.existsSync(p))
  if (whisperPath) console.log('[vox] whisper:', whisperPath)
  else console.error('[vox] whisper not found — STT will fail')
} catch {
  console.error('[vox] whisper detection failed')
}

// ── Conversation history ──────────────────────────────────────────────────────
const history = []


// ── Active agents ─────────────────────────────────────────────────────────────
let activeAgents = 0
let win

function updateAgentCount(delta) {
  activeAgents = Math.max(0, activeAgents + delta)
  win?.webContents.send('agent-status', { count: activeAgents })
}

// ── Agent subprocess ──────────────────────────────────────────────────────────
function spawnAgent(task) {
  if (!claudePath) { console.error('[vox] no claude CLI'); return }
  updateAgentCount(+1)
  const proc = spawn(claudePath, ['--print', '--dangerously-skip-permissions', task], {
    cwd: activeProjectPath,
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  const timer = setTimeout(() => { proc.kill(); updateAgentCount(-1) }, AGENT_TIMEOUT_MS)
  proc.stderr.on('data', d => console.error('[agent]', d.toString().slice(0, 200)))
  proc.on('close', () => { clearTimeout(timer); updateAgentCount(-1) })
}

// ── Execute tool ──────────────────────────────────────────────────────────────
function executeTool(name, input) {
  switch (name) {
    case 'launch_agent':
      spawnAgent(input.task)
      return `Agent lancé : "${input.task}" dans ${activeProjectPath}`
    case 'switch_project':
      activeProjectPath = input.path
      console.log('[vox] switched project to:', input.path)
      return `Projet changé : ${input.path}`
    default:
      return 'Outil inconnu'
  }
}

// ── Helper: run a command async ───────────────────────────────────────────────
function runCommand(cmd, args) {
  return new Promise((resolve, reject) => {
    const proc = spawn(cmd, args, { shell: false })
    let stderr = ''
    proc.stderr?.on('data', d => { stderr += d })
    proc.on('close', code => code === 0 ? resolve() : reject(new Error(`${cmd} exited ${code}: ${stderr.slice(0, 300)}`)))
    proc.on('error', reject)
  })
}

// ── Whisper STT ───────────────────────────────────────────────────────────────
// Renderer sends a WAV buffer (PCM 16kHz mono, encoded in JS) — no ffmpeg needed.
async function transcribeAudio(audioBuffer) {
  const id = Date.now()
  const tmpWav = path.join(os.tmpdir(), `vox_${id}.wav`)
  const tmpTxt = path.join(os.tmpdir(), `vox_${id}.txt`)

  fs.writeFileSync(tmpWav, audioBuffer)

  try {
    await runCommand(whisperPath, [
      tmpWav,
      '--model', 'small',
      '--language', 'fr',
      '--output_format', 'txt',
      '--output_dir', os.tmpdir(),
    ])

    const transcript = fs.existsSync(tmpTxt) ? fs.readFileSync(tmpTxt, 'utf8').trim() : ''
    console.log('[vox] transcript:', transcript)
    return transcript
  } finally {
    for (const f of [tmpWav, tmpTxt]) try { fs.unlinkSync(f) } catch {}
  }
}

// ── Ollama LLM ────────────────────────────────────────────────────────────────
// Uses JSON prompt instead of tool_use API — works with any model incl. gemma3.
async function askOllama(transcript) {
  history.push({ role: 'user', content: transcript })

  const registry = loadRegistry()
  const projectLines = Object.entries(registry)
    .map(([name, p]) => `  - ${name} → ${p}`)
    .join('\n') || '  (aucun)'

  const systemPrompt = `Tu es Vox, un assistant vocal pour développeurs. Tu orchestres des agents de code en arrière-plan.
Projet actif : ${activeProjectPath}

Projets disponibles :
${projectLines}

Réponds TOUJOURS avec un objet JSON valide sur une seule ligne, sans texte autour :
{"action":"none","text":"ta réponse vocale en français (1-2 phrases max)"}

Pour lancer un agent de code en arrière-plan :
{"action":"launch_agent","task":"description précise de la tâche","text":"réponse vocale courte"}

Pour changer de projet (utilise le nom court de la liste) :
{"action":"switch_project","name":"nom-du-projet","text":"réponse vocale courte"}

Tu peux enchaîner plusieurs actions en tableau :
[{"action":"launch_agent","task":"...","text":"Je lance deux agents."},{"action":"launch_agent","task":"...","text":""}]`

  const res = await fetch('http://localhost:11434/v1/chat/completions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model: OLLAMA_MODEL,
      messages: [{ role: 'system', content: systemPrompt }, ...history],
      max_tokens: 300,
      stream: false,
    }),
  })
  if (!res.ok) throw new Error(`Ollama HTTP ${res.status}: ${await res.text()}`)

  const response = await res.json()
  const raw = response.choices?.[0]?.message?.content?.trim() || ''
  history.push({ role: 'assistant', content: raw })
  console.log('[vox] raw ollama:', raw)

  const parsed = parseJSON(raw)

  const actions = Array.isArray(parsed) ? parsed : [parsed]
  let voiceText = ''
  for (const item of actions) {
    if (item.action === 'launch_agent' && item.task) {
      spawnAgent(item.task)
    } else if (item.action === 'switch_project') {
      const registry = loadRegistry()
      const resolved = (item.name && registry[item.name]) || item.path
      if (resolved) {
        activeProjectPath = resolved
        console.log('[vox] switched project to:', activeProjectPath)
      } else {
        console.warn('[vox] project not found:', item.name)
        if (!voiceText) voiceText = `Je ne connais pas le projet "${item.name}". Ajoute-le dans ~/.vox/projects.json.`
      }
    }
    if (item.text && !voiceText) voiceText = item.text
  }
  return voiceText
}

// Robust JSON parser: handles code fences, literal newlines in strings, partial JSON
function parseJSON(raw) {
  // Strip markdown code fences
  let s = raw.replace(/^```(?:json)?\s*/i, '').replace(/\s*```$/, '').trim()

  // Try direct parse
  try { return JSON.parse(s) } catch {}

  // Replace literal newlines inside the string to make it parseable
  s = s.replace(/\n/g, '\\n')
  try { return JSON.parse(s) } catch {}

  // Try extracting the outermost {...} or [...]
  const block = s.match(/(\{[\s\S]*\}|\[[\s\S]*\])/)?.[1]
  if (block) {
    try { return JSON.parse(block) } catch {}
    try { return JSON.parse(block.replace(/\n/g, '\\n')) } catch {}
  }

  // Last resort: pull "text" value out with a regex and discard the rest
  const textMatch = s.match(/"text"\s*:\s*"((?:[^"\\]|\\.)*)"/)?.[1]
  if (textMatch) {
    console.warn('[vox] JSON malformed, extracted text field via regex')
    return { action: 'none', text: textMatch.replace(/\\n/g, ' ') }
  }

  // If the response looks like raw prose (model ignored instructions), speak it
  if (!s.startsWith('{') && !s.startsWith('[')) {
    return { action: 'none', text: s }
  }

  console.warn('[vox] unparseable response, suppressing TTS')
  return { action: 'none', text: '' }
}

// ── TTS: macOS say command ────────────────────────────────────────────────────
function speakWithSay(text) {
  return new Promise((resolve) => {
    // Thomas is a high-quality French voice on macOS
    const proc = spawn('say', ['-v', 'Thomas', text], { shell: false })
    proc.on('close', resolve)
    proc.on('error', resolve)
  })
}

// ── IPC: audio buffer from renderer ──────────────────────────────────────────
ipcMain.on('voice-input', async (_, audioBuffer) => {
  try {
    const transcript = await transcribeAudio(Buffer.from(audioBuffer))
    if (!transcript) {
      win?.webContents.send('speaking-done')
      return
    }

    const reply = await askOllama(transcript)

    if (reply) {
      win?.webContents.send('speaking-start')
      await speakWithSay(reply)
    }
    win?.webContents.send('speaking-done')
  } catch (err) {
    console.error('[vox] pipeline error:', err.message)
    win?.webContents.send('speaking-done')
  }
})

// ── App ready ─────────────────────────────────────────────────────────────────
app.whenReady().then(() => {
  session.defaultSession.setPermissionRequestHandler((_, permission, callback) => {
    callback(permission === 'media')
  })

  const { width: screenW, height: screenH } = require('electron').screen.getPrimaryDisplay().workAreaSize

  win = new BrowserWindow({
    width: WINDOW_WIDTH,
    height: WINDOW_HEIGHT,
    x: Math.round((screenW - WINDOW_WIDTH) / 2),
    y: screenH - WINDOW_HEIGHT - 24,
    frame: false,
    alwaysOnTop: true,
    resizable: false,
    skipTaskbar: true,
    transparent: true,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  })

  win.loadFile('renderer/index.html')

  globalShortcut.register('Alt+Space', () => {
    win.webContents.send('toggle-listening')
  })

  ensureRegistry()
  console.log('[vox] ready — Option+Space to start/stop recording')
})

app.on('will-quit', () => globalShortcut.unregisterAll())
app.on('window-all-closed', () => app.quit())
