const { contextBridge, ipcRenderer } = require('electron')

contextBridge.exposeInMainWorld('vox', {
  // Send recorded audio (ArrayBuffer) to main for transcription + LLM + TTS
  sendAudio: (ab) => ipcRenderer.send('voice-input', Buffer.from(ab)),

  // Agent count updates
  onAgentStatus: (cb) => ipcRenderer.on('agent-status', (_, data) => cb(data)),

  // Hotkey toggle from main
  onToggleListen: (cb) => ipcRenderer.on('toggle-listening', () => cb()),

  // TTS lifecycle from main (say command)
  onSpeakingStart: (cb) => ipcRenderer.on('speaking-start', () => cb()),
  onSpeakingDone:  (cb) => ipcRenderer.on('speaking-done',  () => cb()),
})
