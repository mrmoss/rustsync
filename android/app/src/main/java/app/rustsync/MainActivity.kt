package app.rustsync

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Environment
import android.provider.DocumentsContract
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.viewModels
import androidx.core.content.ContextCompat

class MainActivity : ComponentActivity() {
    private val model: Model by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
        SyncService.start(this)
        setContent {
            RustSyncTheme {
                App(model, hasAllFilesAccess = { hasAllFilesAccess() }, askAllFilesAccess = { askAllFilesAccess() })
            }
        }
    }

    private fun hasAllFilesAccess(): Boolean =
        Build.VERSION.SDK_INT < 30 || Environment.isExternalStorageManager()

    /** Syncing folders anywhere on the phone needs "all files access", granted in Settings. */
    private fun askAllFilesAccess() {
        if (Build.VERSION.SDK_INT >= 30) {
            val uri = Uri.parse("package:$packageName")
            startActivity(Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, uri))
        } else {
            requestPermissions(
                arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE, Manifest.permission.WRITE_EXTERNAL_STORAGE),
                2,
            )
        }
    }
}

/** A folder picked with the system picker, as a path rustsync can use. */
fun treeUriToPath(uri: Uri): String? {
    val docId = try {
        DocumentsContract.getTreeDocumentId(uri)
    } catch (_: Exception) {
        return null
    }
    val parts = docId.split(":", limit = 2)
    val volume = parts[0]
    val rest = parts.getOrElse(1) { "" }
    val root = if (volume == "primary") "/storage/emulated/0" else "/storage/$volume"
    return if (rest.isEmpty()) root else "$root/$rest"
}
