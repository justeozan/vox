const { contextBridge, ipcRenderer } = require('electron')

contextBridge.exposeInMainWorld('vox', {
  sendAudio:      (ab) => ipcRenderer.send('voice-input', Buffer.from(ab)),
  audioDone:      ()   => ipcRenderer.send('audio-done'),
  onAgentStatus:  (cb) => ipcRenderer.on('agent-status',    (_, d) => cb(d)),
  onToggleListen: (cb) => ipcRenderer.on('toggle-listening', ()    => cb()),
  onSpeakingStart:(cb) => ipcRenderer.on('speaking-start',   ()    => cb()),
  onSpeakingDone: (cb) => ipcRenderer.on('speaking-done',    ()    => cb()),
  onPlayWav:      (cb) => ipcRenderer.on('play-wav', (_, p)  => cb(p)),
})
