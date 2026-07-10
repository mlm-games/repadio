package org.mlm.repadio

import android.app.NativeActivity
import android.content.Intent
import android.os.Bundle
import android.util.Log
import java.io.File
import java.io.IOException

class RepadioActivity : NativeActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        intentToBytes(intent)?.let { savePendingIntent(it) }
        super.onCreate(savedInstanceState)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        if (intent.action == Intent.ACTION_VIEW) {
            intentToBytes(intent)?.let { savePendingIntent(it) }
            setIntent(Intent(Intent.ACTION_MAIN))
        }
    }

    private fun readAudioBytes(uri: android.net.Uri): ByteArray? {
        return contentResolver.openInputStream(uri)?.use { it.readBytes() }
    }

    private fun intentToBytes(intent: Intent?): ByteArray? {
        if (intent?.action != Intent.ACTION_VIEW) return null
        val uri = intent.data ?: return null
        return readAudioBytes(uri)
    }

    private fun savePendingIntent(data: ByteArray) {
        try {
            val tmp = File(filesDir, "${PENDING_INTENT_FILE}.tmp")
            val dst = File(filesDir, PENDING_INTENT_FILE)
            tmp.writeBytes(data)
            if (!tmp.renameTo(dst)) throw IOException("rename failed")
            Log.i(TAG, "savePendingIntent: saved ${data.size} bytes")
        } catch (e: Exception) {
            Log.e(TAG, "savePendingIntent failed", e)
        }
    }

    companion object {
        private const val TAG = "Repadio"
        private const val PENDING_INTENT_FILE = "pending_intent"

        @JvmStatic private external fun nativeOnWindowInsets(
            topPx: Float, bottomPx: Float,
            leftPx: Float, rightPx: Float,
            imeBottomPx: Float,
        )
    }
}
