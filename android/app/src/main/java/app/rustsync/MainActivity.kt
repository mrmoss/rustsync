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
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.core.content.ContextCompat

class MainActivity : ComponentActivity() {
    private val model: Model by viewModels()

    // The ordinary permission pop-ups come first; when they are answered, the one that needs a
    // screen in Android's settings (all files access, Android 11 and newer).
    private val runtimePermissions =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
            if (Build.VERSION.SDK_INT >= 30 && !hasAllFilesAccess()) askAllFilesAccess()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        SyncService.start(this)
        setContent {
            RustSyncTheme {
                var setup by remember { mutableStateOf(needsNotifications() || needsCamera() || !hasAllFilesAccess()) }
                if (setup) {
                    AlertDialog(
                        onDismissRequest = { setup = false },
                        title = { Text("Allow what RustSync needs") },
                        text = {
                            Text(
                                buildString {
                                    if (needsNotifications()) {
                                        append("• Notifications: RustSync shows one so Android keeps it running in the background.\n\n")
                                    }
                                    if (needsCamera()) {
                                        append("• Camera: to scan the QR code when you pair another device.\n\n")
                                    }
                                    if (!hasAllFilesAccess()) {
                                        append("• Access to your files: to sync folders such as Documents or Pictures. Android asks for this on a separate settings screen.\n\n")
                                    }
                                    append("You can change any of these later in Android's settings.")
                                },
                            )
                        },
                        confirmButton = {
                            TextButton(onClick = {
                                setup = false
                                requestEverything()
                            }) { Text("Continue") }
                        },
                        dismissButton = { TextButton(onClick = { setup = false }) { Text("Not now") } },
                    )
                }
                App(model, hasAllFilesAccess = { hasAllFilesAccess() }, askAllFilesAccess = { askAllFilesAccess() })
            }
        }
    }

    private fun granted(permission: String): Boolean =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    private fun needsNotifications(): Boolean =
        Build.VERSION.SDK_INT >= 33 && !granted(Manifest.permission.POST_NOTIFICATIONS)

    private fun needsCamera(): Boolean = !granted(Manifest.permission.CAMERA)

    /** Before Android 11 "all files" is the ordinary storage permission. */
    private fun needsOldStorage(): Boolean =
        Build.VERSION.SDK_INT < 30 &&
            (!granted(Manifest.permission.READ_EXTERNAL_STORAGE) || !granted(Manifest.permission.WRITE_EXTERNAL_STORAGE))

    private fun requestEverything() {
        val missing = ArrayList<String>()
        if (needsNotifications()) missing.add(Manifest.permission.POST_NOTIFICATIONS)
        if (needsCamera()) missing.add(Manifest.permission.CAMERA)
        if (needsOldStorage()) {
            missing.add(Manifest.permission.READ_EXTERNAL_STORAGE)
            missing.add(Manifest.permission.WRITE_EXTERNAL_STORAGE)
        }
        if (missing.isNotEmpty()) {
            runtimePermissions.launch(missing.toTypedArray())
        } else if (!hasAllFilesAccess()) {
            askAllFilesAccess()
        }
    }

    private fun hasAllFilesAccess(): Boolean =
        if (Build.VERSION.SDK_INT >= 30) Environment.isExternalStorageManager() else !needsOldStorage()

    /** Syncing folders anywhere on the phone needs "all files access", granted in Settings. */
    private fun askAllFilesAccess() {
        if (Build.VERSION.SDK_INT >= 30) {
            val uri = Uri.parse("package:$packageName")
            startActivity(Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, uri))
        } else if (needsOldStorage()) {
            runtimePermissions.launch(
                arrayOf(Manifest.permission.READ_EXTERNAL_STORAGE, Manifest.permission.WRITE_EXTERNAL_STORAGE),
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
