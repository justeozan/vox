const { app, BrowserWindow, session } = require('electron')
const { execSync } = require('child_process')

// T1 spike: validate 3 things before building anything else
// 1. claude CLI is findable
// 2. Web Speech API works in Electron renderer
// 3. IPC can carry a transcript from renderer to main

let claudePath
try {
  claudePath = execSync('which claude', { shell: true }).toString().trim()
  console.log('✅ claude found at:', claudePath)
} catch {
  console.error('❌ claude not found in PATH — spike failed at step 1')
  process.exit(1)
}

app.whenReady().then(() => {
  session.defaultSession.setPermissionRequestHandler((_, permission, cb) => {
    cb(permission === 'media')
  })

  const win = new BrowserWindow({
    width: 600,
    height: 300,
    webPreferences: { nodeIntegration: false, contextIsolation: true },
  })

  win.loadURL('data:text/html,' + encodeURIComponent(`
    <!DOCTYPE html>
    <html>
    <body style="background:#111;color:#eee;font-family:monospace;padding:20px">
      <h3>Vox Spike — Web Speech API Test</h3>
      <p id="status">Click START to begin...</p>
      <button onclick="start()" style="padding:8px 16px;font-size:14px">START</button>
      <pre id="log" style="margin-top:16px;color:#4ade80"></pre>
      <script>
        function log(msg) {
          document.getElementById('log').textContent += msg + '\\n'
          console.log(msg)
        }
        function start() {
          if (!('webkitSpeechRecognition' in window) && !('SpeechRecognition' in window)) {
            log('❌ SpeechRecognition not available in this Electron build')
            return
          }
          log('✅ SpeechRecognition API available')
          const SR = window.SpeechRecognition || window.webkitSpeechRecognition
          const r = new SR()
          r.continuous = false
          r.interimResults = false
          r.lang = 'fr-FR'
          r.onstart = () => { log('✅ Microphone started — speak now'); document.getElementById('status').textContent = 'Listening...' }
          r.onresult = (e) => { log('✅ Transcript: ' + e.results[0][0].transcript) }
          r.onerror = (e) => { log('❌ Error: ' + e.error) }
          r.onend = () => { log('Recognition ended'); document.getElementById('status').textContent = 'Done' }
          // getUserMedia triggers the macOS TCC permission dialog
          // SpeechRecognition alone doesn't prompt on macOS
          navigator.mediaDevices.getUserMedia({ audio: true })
            .then(stream => {
              stream.getTracks().forEach(t => t.stop())
              log('✅ Microphone permission granted')
              r.start()
            })
            .catch(err => {
              log('❌ getUserMedia denied: ' + err.message)
            })
        }
      </script>
    </body>
    </html>
  `))

  win.webContents.openDevTools({ mode: 'detach' })
})
