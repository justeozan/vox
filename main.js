const { app, BrowserWindow, globalShortcut, ipcMain, session } = require('electron')
const { execSync, spawn } = require('child_process')
const path = require('path')
const fs = require('fs')
const os = require('os')

// ── Config ────────────────────────────────────────────────────────────────────
const WINDOW_WIDTH = 480
const WINDOW_HEIGHT = 56
const AGENT_TIMEOUT_MS = 120_000
const OLLAMA_MODEL          = process.env.VOX_MODEL || 'qwen2.5:3b'
const REGISTRY_PATH         = path.join(os.homedir(), '.vox', 'projects.json')
const CONDUCTOR_WORKSPACES  = path.join(os.homedir(), 'conductor', 'workspaces')
const CLAUDE_PROJECTS_DIR   = path.join(os.homedir(), '.claude', 'projects')

// Language mode — controls TTS voice, STT language, and LLM prompt language.
// VOX_LANG=en  →  English (af_heart voice, Kokoro lang 'a', whisper 'en')
// VOX_LANG=fr  →  French (ff_siwis voice, Kokoro lang 'f', whisper 'fr')  ← default
const VOX_LANG = (process.env.VOX_LANG || 'fr').toLowerCase()
const LANG_CONFIG = {
  fr: { sttLang: 'fr', kokoroLang: 'f', kokoroVoice: 'ff_siwis', label: 'français' },
  en: { sttLang: 'en', kokoroLang: 'a', kokoroVoice: 'af_heart',  label: 'english'  },
}[VOX_LANG] || { sttLang: 'fr', kokoroLang: 'f', kokoroVoice: 'ff_siwis', label: 'français' }

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

// ── Conductor workspace activity ──────────────────────────────────────────────
let _activityCache = null
let _activityCacheAt = 0

function readRecentActivity(maxItems = 6) {
  // Cache for 90s — avoids filesystem scan on every LLM call
  if (_activityCache && Date.now() - _activityCacheAt < 90_000) {
    return _activityCache.slice(0, maxItems)
  }

  const results = []
  try {
    const projects = fs.readdirSync(CONDUCTOR_WORKSPACES)
    for (const project of projects) {
      const projectDir = path.join(CONDUCTOR_WORKSPACES, project)
      let cities = []
      try { cities = fs.readdirSync(projectDir).filter(c => !c.startsWith('.')) } catch { continue }

      for (const city of cities) {
        const wsPath = path.join(projectDir, city)
        try { if (!fs.statSync(wsPath).isDirectory()) continue } catch { continue }

        // ~/.claude/projects dir is the absolute path with / replaced by -
        const claudeDir = path.join(CLAUDE_PROJECTS_DIR, wsPath.split('/').join('-'))
        if (!fs.existsSync(claudeDir)) continue

        // Find newest JSONL in this project dir (skip subagent folders)
        let latestFile = null, latestMtime = 0
        try {
          for (const f of fs.readdirSync(claudeDir)) {
            if (!f.endsWith('.jsonl')) continue
            const fp = path.join(claudeDir, f)
            const mt = fs.statSync(fp).mtimeMs
            if (mt > latestMtime) { latestMtime = mt; latestFile = fp }
          }
        } catch {}
        if (!latestFile) continue

        // Extract last meaningful assistant text (skip tool_use, interrupted lines)
        let lastText = ''
        try {
          const lines = fs.readFileSync(latestFile, 'utf8').split('\n')
          for (const line of lines) {
            try {
              const obj = JSON.parse(line)
              const msg = obj.message || {}
              if (msg.role !== 'assistant') continue
              const content = msg.content || []
              const text = (Array.isArray(content)
                ? content.filter(c => c.type === 'text').map(c => c.text).join(' ')
                : String(content)
              ).trim()
              if (text.length > 8 && !text.startsWith('[Request')) lastText = text.slice(0, 150)
            } catch {}
          }
        } catch {}

        results.push({ project, city, mtime: latestMtime, lastText })
      }
    }
  } catch (err) {
    console.warn('[vox] readRecentActivity error:', err.message)
  }

  _activityCache = results.sort((a, b) => b.mtime - a.mtime)
  _activityCacheAt = Date.now()
  return _activityCache.slice(0, maxItems)
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

// ── mlx-whisper STT daemon ────────────────────────────────────────────────────
let sttDaemon          = null
let sttReady           = false
let sttPendingResolve  = null

function startSttDaemon() {
  const script = path.join(__dirname, 'vox_stt.py')
  if (!fs.existsSync(script)) { console.warn('[vox] vox_stt.py not found — using whisper CLI'); return }

  // Prefer the Python that has openai-whisper installed (same env as the whisper CLI)
  const whisperBinDir = whisperPath ? path.dirname(whisperPath) : null
  const rawCandidates = [
    whisperBinDir && path.join(whisperBinDir, 'python3.9'),
    whisperBinDir && path.join(whisperBinDir, 'python3'),
    execSync('which python3.9 2>/dev/null || true', { shell: true }).toString().trim(),
    execSync('which python3.10 2>/dev/null || true', { shell: true }).toString().trim(),
    execSync('which python3 2>/dev/null || true', { shell: true }).toString().trim(),
    path.join(os.homedir(), '.vox', 'venv', 'bin', 'python'),
  ].filter(Boolean)

  // Pick first Python that can actually import whisper (openai-whisper)
  const python = rawCandidates.find(p => {
    if (!p || !fs.existsSync(p)) return false
    try { execSync(`"${p}" -c "import whisper"`, { shell: false, stdio: 'ignore' }); return true } catch { return false }
  })
  if (!python) { console.warn('[vox] no Python with openai-whisper found — using whisper CLI'); return }

  console.log(`[vox] starting STT daemon (lang: ${LANG_CONFIG.sttLang})...`)
  sttDaemon = spawn(python, [script], {
    shell: false,
    stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, VOX_STT_LANG: LANG_CONFIG.sttLang },
  })

  let buf = ''
  sttDaemon.stdout.on('data', (d) => {
    buf += d.toString()
    let nl
    while ((nl = buf.indexOf('\n')) !== -1) {
      const line = buf.slice(0, nl).trim()
      buf = buf.slice(nl + 1)
      if (line === 'ready') {
        sttReady = true
        console.log('[vox] mlx-whisper STT ready')
      } else {
        const resolve = sttPendingResolve
        sttPendingResolve = null
        resolve?.(line === 'EMPTY' || line.startsWith('ERROR:') ? '' : line)
      }
    }
  })
  sttDaemon.stderr.on('data', d => console.log('[stt]', d.toString().slice(0, 200)))
  sttDaemon.on('close', () => { sttDaemon = null; sttReady = false })
}

function transcribeWithDaemon(wavPath) {
  return new Promise((resolve) => {
    if (!sttDaemon || !sttReady) return resolve(null)
    sttPendingResolve = resolve
    sttDaemon.stdin.write(`${wavPath}\n`)
  })
}

// ── Conversation history ──────────────────────────────────────────────────────
const history = []

// ── Tools for models that support native tool calling (qwen2.5, llama3…) ──────
const VOX_TOOLS = [
  {
    type: 'function',
    function: {
      name: 'launch_agent',
      description: 'Lance un agent Claude pour effectuer une tâche de développement en arrière-plan',
      parameters: {
        type: 'object',
        properties: {
          task: { type: 'string', description: 'Description détaillée de la tâche' },
          text: { type: 'string', description: 'Réponse vocale courte à prononcer (1-2 phrases)' },
        },
        required: ['task', 'text'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'switch_project',
      description: 'Change le projet actif par son nom dans le registre ~/.vox/projects.json',
      parameters: {
        type: 'object',
        properties: {
          name: { type: 'string', description: 'Nom du projet dans le registre' },
          text: { type: 'string', description: 'Réponse vocale courte' },
        },
        required: ['name', 'text'],
      },
    },
  },
]

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

// ── STT ────────────────────────────────────────────────────────────────────────
// Renderer sends a WAV buffer (PCM 16kHz mono, encoded in JS) — no ffmpeg needed.
async function transcribeAudio(audioBuffer) {
  const id = Date.now()
  const tmpWav = path.join(os.tmpdir(), `vox_${id}.wav`)
  const tmpTxt = path.join(os.tmpdir(), `vox_${id}.txt`)

  fs.writeFileSync(tmpWav, audioBuffer)

  try {
    // Prefer mlx-whisper daemon (Apple Silicon Neural Engine — fast + accurate)
    if (sttReady) {
      const transcript = await transcribeWithDaemon(tmpWav)
      if (transcript !== null) {
        console.log('[vox] transcript (mlx):', transcript)
        return transcript
      }
    }

    // Fallback: whisper CLI
    if (!whisperPath) throw new Error('no STT available')
    await runCommand(whisperPath, [
      tmpWav,
      '--model', 'small',
      '--language', 'fr',
      '--output_format', 'txt',
      '--output_dir', os.tmpdir(),
    ])
    const transcript = fs.existsSync(tmpTxt) ? fs.readFileSync(tmpTxt, 'utf8').trim() : ''
    console.log('[vox] transcript (cli):', transcript)
    return transcript
  } finally {
    for (const f of [tmpWav, tmpTxt]) try { fs.unlinkSync(f) } catch {}
  }
}

// ── Startup brief ──────────────────────────────────────────────────────────────
let startupBriefDone = false

// Extract first clean sentence from raw Claude lastText (avoids truncated words leaking into TTS)
function cleanActivitySnippet(raw) {
  let t = raw
    .replace(/```[\s\S]*?```/g, '')   // code blocks
    .replace(/`[^`\n]+`/g, '')        // inline code
    .replace(/\*{1,3}([^*\n]+)\*{1,3}/g, '$1')
    .replace(/_{1,2}([^_\n]+)_{1,2}/g, '$1')
    .replace(/^#{1,6}\s+/gm, '')
    .replace(/[|#]/g, '')
    .replace(/\s+/g, ' ')
    .trim()
  // Keep only first complete sentence (ends with . ! ?)
  const m = t.match(/^(.{10,120}?[.!?])/)
  return m ? m[1].trim() : t.slice(0, 80).trim()
}

async function speakStartupBrief() {
  if (startupBriefDone) return
  startupBriefDone = true

  const activity = readRecentActivity(4)
  if (!activity.length) return

  const now = Date.now()
  const when = (ms) => {
    const d = (now - ms) / 86400000
    if (d < 0.5) return 'aujourd\'hui'
    if (d < 1.5) return 'hier'
    if (d < 8) return `il y a ${Math.round(d)} jours`
    return 'récemment'
  }

  // Context: name + timing + first clean sentence of last activity
  const ctx = activity.slice(0, 3).map(a => {
    const snippet = a.lastText ? cleanActivitySnippet(a.lastText) : ''
    return snippet
      ? `- ${a.project} (${when(a.mtime)}) : ${snippet}`
      : `- ${a.project} (${when(a.mtime)})`
  }).join('\n')

  try {
    const res = await fetch('http://localhost:11434/v1/chat/completions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model: OLLAMA_MODEL,
        messages: [
          {
            role: 'system',
            content: [
              'Tu es Vox, assistant vocal d\'un développeur solo. Tu parles comme un collègue bienveillant.',
              'Réponds en deux phrases maximum, en français oral, sans listes ni tirets ni ponctuation spéciale.',
              'Phrase 1 : résume ce qui s\'est passé sur les projets récents.',
              'Phrase 2 : recommande concrètement quel projet reprendre aujourd\'hui et pourquoi.',
              'Exemple : "Bonjour ! Cardex avance bien avec une pull request en cours, et energy-dashboard avait un bug hier.',
              'Je te recommande de commencer par Cardex pour débloquer la pull request."',
            ].join(' '),
          },
          {
            role: 'user',
            content: `Activité récente :\n${ctx}\n\nFais le briefing matinal.`,
          },
        ],
        max_tokens: 120,
        stream: false,
      }),
    })
    if (!res.ok) return
    const data = await res.json()
    const brief = (data.choices?.[0]?.message?.content || '').trim().replace(/^["']|["']$/g, '')
    if (brief) await speak(brief)
  } catch (err) {
    console.warn('[vox] startup brief failed:', err.message)
  }
}

// ── Ollama LLM ────────────────────────────────────────────────────────────────
async function askOllama(transcript) {
  history.push({ role: 'user', content: transcript })

  const registry = loadRegistry()
  const projectLines = Object.entries(registry)
    .map(([name, p]) => `  - ${name} → ${p}`)
    .join('\n') || '  (aucun)'

  const activity = readRecentActivity(5)
  const activityLines = activity.length
    ? activity.map(a => {
        const daysAgo = Math.round((Date.now() - a.mtime) / 86400000)
        const when = daysAgo === 0 ? 'aujourd\'hui' : daysAgo === 1 ? 'hier' : `il y a ${daysAgo}j`
        return `  ${a.project}/${a.city} (${when}): ${a.lastText.slice(0, 80)}`
      }).join('\n')
    : '  (aucune)'

  const baseSystem = VOX_LANG === 'en'
    ? `You are Vox, a voice AI assistant for developers.
ABSOLUTE RULES:
- Always reply in English, never in French
- Maximum 1 short sentence (15 words max), always
- No lists, no explanations, no unnecessary punctuation

Active project: ${activeProjectPath}
Available projects: ${projectLines}

Recent workspace activity:
${activityLines}`
    : `Tu es Vox, un assistant vocal IA pour développeurs.
RÈGLES ABSOLUES — ne jamais déroger :
- Toujours en français, jamais en anglais
- Maximum 1 phrase courte (15 mots max), toujours
- Pas de liste, pas d'explication, pas de ponctuation inutile

Projet actif : ${activeProjectPath}
Projets conductor disponibles : ${projectLines}

Activité récente des workspaces :
${activityLines}`

  // ── Attempt 1 : native tools API (qwen2.5:3b, llama3…) ──────────────────────
  try {
    const res = await fetch('http://localhost:11434/v1/chat/completions', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        model: OLLAMA_MODEL,
        messages: [{ role: 'system', content: baseSystem }, ...history],
        tools: VOX_TOOLS,
        max_tokens: 300,
        stream: false,
      }),
    })
    if (!res.ok) throw new Error(`HTTP ${res.status}`)

    const data = await res.json()
    const msg = data.choices?.[0]?.message
    const toolCalls = msg?.tool_calls

    if (toolCalls?.length) {
      let voiceText = ''
      for (const tc of toolCalls) {
        const name = tc.function?.name
        const args = typeof tc.function?.arguments === 'string'
          ? JSON.parse(tc.function.arguments)
          : (tc.function?.arguments || {})
        if (!voiceText && args.text) voiceText = args.text
        if (name === 'launch_agent' && args.task) {
          spawnAgent(args.task)
        } else if (name === 'switch_project' && args.name) {
          const resolved = registry[args.name]
          if (resolved) {
            activeProjectPath = resolved
            console.log('[vox] switched project to:', activeProjectPath)
          } else {
            voiceText = voiceText || `Je ne connais pas le projet "${args.name}".`
          }
        }
      }
      const reply = voiceText || ''
      history.push({ role: 'assistant', content: reply })
      console.log('[vox] tools response:', reply)
      return reply
    }

    // Model responded with plain text — may still be a text-format tool call.
    // qwen2.5+Ollama sometimes emits: "switch_project {"name":"x","text":"y"}"
    const text = msg?.content?.trim()
    if (text) {
      const textTool = text.match(/^(\w+)\s*(\{[\s\S]*\})\s*$/)
      if (textTool) {
        try {
          const [, toolName, argsStr] = textTool
          const args = JSON.parse(argsStr)
          let voiceText = args.text || ''
          if (toolName === 'launch_agent' && args.task) {
            spawnAgent(args.task)
          } else if (toolName === 'switch_project' && args.name) {
            const resolved = registry[args.name]
            if (resolved) {
              activeProjectPath = resolved
              console.log('[vox] switched project to:', activeProjectPath)
            } else {
              voiceText = voiceText || `Je ne connais pas le projet "${args.name}".`
            }
          }
          history.push({ role: 'assistant', content: voiceText })
          console.log('[vox] text-tool response:', toolName, voiceText)
          return voiceText
        } catch {}
      }
      history.push({ role: 'assistant', content: text })
      console.log('[vox] text response:', text)
      return text
    }
  } catch (err) {
    console.warn('[vox] tools API failed, using JSON prompt:', err.message)
  }

  // ── Attempt 2 : JSON prompt fallback (gemma3, any model) ─────────────────────
  const jsonSystem = baseSystem + `

Réponds TOUJOURS avec un objet JSON valide sur une seule ligne, sans aucun texte autour.
Exemples :
{"action":"none","text":"Oui, je t'entends bien."}
{"action":"launch_agent","task":"Corriger les tests unitaires qui échouent","text":"Je lance l'agent sur les tests."}
{"action":"switch_project","name":"mon-app","text":"Je passe sur mon-app."}`

  const res2 = await fetch('http://localhost:11434/v1/chat/completions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      model: OLLAMA_MODEL,
      messages: [{ role: 'system', content: jsonSystem }, ...history],
      max_tokens: 300,
      stream: false,
    }),
  })
  if (!res2.ok) throw new Error(`Ollama HTTP ${res2.status}: ${await res2.text()}`)

  const data2 = await res2.json()
  const raw = data2.choices?.[0]?.message?.content?.trim() || ''
  history.push({ role: 'assistant', content: raw })
  console.log('[vox] raw ollama (json):', raw)

  const parsed = parseJSON(raw)
  const actions = Array.isArray(parsed) ? parsed : [parsed]
  let voiceText = ''
  for (const item of actions) {
    if (item.action === 'launch_agent' && item.task) {
      spawnAgent(item.task)
    } else if (item.action === 'switch_project') {
      const resolved = (item.name && registry[item.name]) || item.path
      if (resolved) {
        activeProjectPath = resolved
        console.log('[vox] switched project to:', activeProjectPath)
      } else {
        voiceText = voiceText || `Je ne connais pas le projet "${item.name}". Ajoute-le dans ~/.vox/projects.json.`
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

// ── TTS: Kokoro daemon (natural) + say fallback (synthetic) ──────────────────
let currentPlayProc = null  // afplay or say — killable for barge-in
let kokoroDaemon = null
let kokoroReady  = false
let kokoroPendingResolve = null  // resolves when daemon acks the current synthesis

function startKokoroDaemon() {
  const script = path.join(__dirname, 'vox_tts.py')
  if (!fs.existsSync(script)) return

  // Prefer the ~/.vox/venv Python (has kokoro), fallback to system python3
  const pythonCandidates = [
    path.join(os.homedir(), '.vox', 'venv', 'bin', 'python'),
    execSync('which python3.11 2>/dev/null || true', { shell: true }).toString().trim(),
    execSync('which python3 2>/dev/null || true', { shell: true }).toString().trim(),
  ]
  const python = pythonCandidates.find(p => p && fs.existsSync(p))
  if (!python) return

  console.log(`[vox] starting Kokoro TTS daemon (${LANG_CONFIG.kokoroVoice})...`)
  const venvPath = path.join(os.homedir(), '.vox', 'venv')
  kokoroDaemon = spawn(python, [script], {
    shell: false,
    stdio: ['pipe', 'pipe', 'pipe'],
    env: {
      ...process.env,
      VIRTUAL_ENV: venvPath,
      VOX_KOKORO_LANG:  LANG_CONFIG.kokoroLang,
      VOX_KOKORO_VOICE: process.env.VOX_KOKORO_VOICE || LANG_CONFIG.kokoroVoice,
    },
  })

  let buf = ''
  kokoroDaemon.stdout.on('data', (d) => {
    buf += d.toString()
    let nl
    while ((nl = buf.indexOf('\n')) !== -1) {
      const line = buf.slice(0, nl).trim()
      buf = buf.slice(nl + 1)
      if (line === 'ready') {
        kokoroReady = true
        console.log('[vox] Kokoro TTS ready')
        setTimeout(speakStartupBrief, 800)
      } else if (line.startsWith('ok:') || line.startsWith('error:')) {
        const resolve = kokoroPendingResolve
        kokoroPendingResolve = null
        resolve?.(line.startsWith('ok:') ? line.slice(3) : null)
      }
    }
  })
  kokoroDaemon.stderr.on('data', d => console.log('[kokoro]', d.toString().slice(0, 200)))
  kokoroDaemon.on('close', () => {
    kokoroDaemon = null
    kokoroReady = false
    const pending = kokoroPendingResolve
    kokoroPendingResolve = null
    pending?.(null)
  })
}

function synthesizeKokoro(text) {
  return new Promise((resolve) => {
    if (!kokoroDaemon || !kokoroReady) return resolve(null)
    const outPath = path.join(os.tmpdir(), `vox_tts_${Date.now()}.wav`)
    kokoroPendingResolve = resolve
    kokoroDaemon.stdin.write(`${outPath}\t${text}\n`)
    // Safety timeout: fall back to say if daemon stalls
    setTimeout(() => {
      if (kokoroPendingResolve === resolve) {
        kokoroPendingResolve = null
        resolve(null)
      }
    }, 15_000)
  })
}

async function speakWithSay(text) {
  return new Promise((resolve) => {
    currentPlayProc = spawn('say', ['-v', 'Eddy (Français (France))', text], { shell: false })
    currentPlayProc.on('close', () => { currentPlayProc = null; resolve() })
    currentPlayProc.on('error', () => { currentPlayProc = null; resolve() })
  })
}

async function speak(text) {
  console.log('[vox] 🔊', text)
  // Wait up to 8s for Kokoro to finish loading on first call
  if (!kokoroReady) {
    await new Promise(resolve => {
      const check = setInterval(() => { if (kokoroReady) { clearInterval(check); resolve() } }, 100)
      setTimeout(() => { clearInterval(check); resolve() }, 8000)
    })
  }
  if (kokoroReady) {
    const wavPath = await synthesizeKokoro(text)
    if (wavPath) {
      win?.webContents.send('speaking-start')
      win?.webContents.send('play-wav', wavPath)   // renderer plays → echo cancellation works
      await new Promise(resolve => ipcMain.once('audio-done', resolve))
      try { fs.unlinkSync(wavPath) } catch {}
      win?.webContents.send('speaking-done')
      return
    }
  }
  // Fallback: say (no barge-in — afplay bypasses browser echo cancellation)
  win?.webContents.send('speaking-start')
  await speakWithSay(text)
  win?.webContents.send('speaking-done')
}

ipcMain.on('interrupt-speaking', () => {
  if (currentPlayProc) {
    currentPlayProc.kill('SIGTERM')
    currentPlayProc = null
  }
  kokoroPendingResolve = null
  win?.webContents.send('speaking-done')
})

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
      await speak(reply)   // speak() sends speaking-start/done internally
    } else {
      win?.webContents.send('speaking-done')
    }
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
  startKokoroDaemon()
  startSttDaemon()
  console.log('[vox] ready — Option+Space to activate')
})

app.on('will-quit', () => globalShortcut.unregisterAll())
app.on('window-all-closed', () => app.quit())
