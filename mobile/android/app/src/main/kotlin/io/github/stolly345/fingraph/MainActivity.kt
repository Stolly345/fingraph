package io.github.stolly345.fingraph

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.view.WindowManager
import io.flutter.embedding.android.FlutterFragmentActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

/// FlutterFragmentActivity for the fingerprint prompt; plus three small pieces the app asks for over
/// channels: hiding the window from screenshots, a PDF shared to FinGraph, and the system file picker.
class MainActivity : FlutterFragmentActivity() {
    private var shareChannel: MethodChannel? = null
    private var pending: Map<String, Any>? = null
    private var pickResult: MethodChannel.Result? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        val messenger = flutterEngine.dartExecutor.binaryMessenger
        MethodChannel(messenger, "fingraph/window").setMethodCallHandler { call, result ->
            if (call.method == "secure") {
                if (call.arguments == true) window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
                else window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
                result.success(null)
            } else result.notImplemented()
        }
        shareChannel = MethodChannel(messenger, "fingraph/share").also { ch ->
            ch.setMethodCallHandler { call, result ->
                when (call.method) {
                    "take" -> { result.success(pending); pending = null }
                    "pick" -> {
                        pickResult = result
                        val i = Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("application/pdf")
                        startActivityForResult(i, PICK_PDF)
                    }
                    else -> result.notImplemented()
                }
            }
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) pending = read(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        val f = read(intent) ?: return
        shareChannel?.invokeMethod("shared", f) ?: run { pending = f }
    }

    @Deprecated("startActivityForResult is enough for one picker")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK_PDF) return
        val r = pickResult ?: return
        pickResult = null
        val uri = data?.data
        r.success(if (resultCode == Activity.RESULT_OK && uri != null) file(uri) else null)
    }

    /// A PDF shared to FinGraph (Share → FinGraph, or Open with).
    private fun read(intent: Intent?): Map<String, Any>? {
        intent ?: return null
        val uri: Uri? = when (intent.action) {
            Intent.ACTION_SEND -> @Suppress("DEPRECATION") (intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM))
            Intent.ACTION_VIEW -> intent.data
            else -> null
        }
        return uri?.let { file(it) }
    }

    private fun file(uri: Uri): Map<String, Any>? = try {
        var name = "statement.pdf"
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) c.getString(0)?.let { name = it }
        }
        val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
        // statements are small; anything huge isn't one
        if (bytes == null || bytes.size > 100 * 1024 * 1024) null else mapOf("name" to name, "bytes" to bytes)
    } catch (e: Exception) {
        null
    }

    companion object {
        private const val PICK_PDF = 4711
    }
}
