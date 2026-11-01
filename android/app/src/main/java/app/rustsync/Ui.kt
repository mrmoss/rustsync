package app.rustsync

import android.graphics.Bitmap
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.qrcode.decoder.ErrorCorrectionLevel
import com.journeyapps.barcodescanner.BarcodeEncoder
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions

private val tabs = listOf("Home" to Glyph.Home, "Libraries" to Glyph.Folder, "Devices" to Glyph.Devices)

@Composable
fun App(model: Model, hasAllFilesAccess: () -> Boolean, askAllFilesAccess: () -> Unit) {
    var tab by remember { mutableIntStateOf(0) } // Home first, and the one you land on
    val snackbar = remember { SnackbarHostState() }
    val message = model.message
    LaunchedEffect(message) {
        if (message != null) {
            snackbar.showSnackbar(message)
            model.message = null
        }
    }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        bottomBar = {
            NavigationBar {
                tabs.forEachIndexed { i, (label, icon) ->
                    NavigationBarItem(
                        selected = tab == i,
                        onClick = { tab = i },
                        icon = { GlyphIcon(icon) },
                        label = { Text(label) },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding).fillMaxSize()) {
            val status = model.status
            if (status == null) {
                Offline(model.offline ?: "")
            } else {
                when (tab) {
                    0 -> ThisDevice(model, status, hasAllFilesAccess, askAllFilesAccess)
                    1 -> Libraries(model, status)
                    else -> Devices(model, status)
                }
            }
        }
    }
    PairingDialogs(model)
}

@Composable
private fun Page(title: String, subtitle: String, content: @Composable () -> Unit) {
    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(title, style = MaterialTheme.typography.headlineMedium)
        Text(subtitle, style = MaterialTheme.typography.bodyMedium)
        content()
    }
}

@Composable
private fun Offline(reason: String) {
    Column(Modifier.fillMaxSize().padding(24.dp), verticalArrangement = Arrangement.Center) {
        CircularProgressIndicator()
        Spacer(Modifier.height(12.dp))
        Text("RustSync is starting…")
        if (reason.isNotBlank() && reason != "Starting…") Text(reason, style = MaterialTheme.typography.bodySmall)
    }
}

// ---------------------------------------------------------------- Home

@Composable
private fun ThisDevice(model: Model, status: Status, hasAllFilesAccess: () -> Boolean, askAllFilesAccess: () -> Unit) {
    var allFiles by remember { mutableStateOf(hasAllFilesAccess()) }
    LaunchedEffect(Unit) { allFiles = hasAllFilesAccess() }
    val clipboard = LocalClipboardManager.current
    Page("Home", "This phone: how other devices see it.") {
        RsCard(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(status.name, style = MaterialTheme.typography.titleLarge)
                Text("● running", color = MaterialTheme.colorScheme.primary)
                Text("Listening on ${status.listen}", style = MaterialTheme.typography.bodyMedium)
                Text("Public key", style = MaterialTheme.typography.labelMedium)
                Text(status.key, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
                TextButton(onClick = { clipboard.setText(AnnotatedString(status.key)) }) { Text("Copy key") }
                Text("RustSync ${Core.version()}", style = MaterialTheme.typography.bodySmall)
            }
        }
        if (!allFiles) {
            RsCard(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Allow access to your files", style = MaterialTheme.typography.titleMedium)
                    Text(
                        "To sync folders like Documents or Pictures, RustSync needs “all files access”. " +
                            "Android asks for it on a separate settings screen.",
                    )
                    Button(onClick = {
                        askAllFilesAccess()
                    }) { Text("Open settings") }
                    TextButton(onClick = { allFiles = hasAllFilesAccess() }) { Text("I allowed it") }
                }
            }
        }
        RsCard(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Settings", style = MaterialTheme.typography.titleMedium)
                Text("Port this phone listens on for the other devices:")
                var port by remember(status.port) { mutableStateOf(status.port.toString()) }
                val parsed = port.trim().toIntOrNull()?.takeIf { it in 1..65535 }
                OutlinedTextField(
                    value = port,
                    onValueChange = { port = it.filter { c -> c.isDigit() }.take(5) },
                    singleLine = true,
                    label = { Text("Port") },
                    isError = port.isNotBlank() && parsed == null,
                )
                Text(
                    "The default is 3032. Other devices that connect to this phone need the new port too " +
                        "(they can use “Change address”).",
                    style = MaterialTheme.typography.bodySmall,
                )
                Button(
                    enabled = parsed != null && parsed != status.port,
                    onClick = { parsed?.let { model.setPort(it) } },
                ) { Text("Save port") }
            }
        }
        val context = LocalContext.current
        val version = remember {
            try {
                context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: ""
            } catch (_: Exception) {
                ""
            }
        }
        RsCard(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("About", style = MaterialTheme.typography.titleMedium)
                Text(if (version.isEmpty()) "RustSync" else "RustSync $version")
                Text("Made by Michael Moss with the aid of clankers.")
                Text(
                    "Free software, released under the GNU General Public License, version 2 (GPL-2.0-only). " +
                        "It comes with no warranty.",
                    style = MaterialTheme.typography.bodySmall,
                )
                Text("github.com/mrmoss/rustsync", style = MaterialTheme.typography.bodySmall)
            }
        }
        Text(
            "Keep the notification: it is what lets Android leave RustSync running in the background.",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

// ---------------------------------------------------------------- Libraries

@Composable
private fun rememberFolderPicker(onPicked: (String) -> Unit): () -> Unit {
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocumentTree()) { uri ->
        if (uri != null) treeUriToPath(uri)?.let(onPicked)
    }
    return { launcher.launch(null) }
}

@Composable
private fun Libraries(model: Model, status: Status) {
    var creating by remember { mutableStateOf(false) }
    var accepting by remember { mutableStateOf<OfferInfo?>(null) }
    var sharing by remember { mutableStateOf<LibraryInfo?>(null) }
    var removing by remember { mutableStateOf<LibraryInfo?>(null) }

    Page("Libraries", "Named folders kept in sync between your devices.") {
        for (offer in status.offers) {
            RsCard(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    val what = if (offer.backup) "keep an encrypted backup of “${offer.name}”" else "sync “${offer.name}”"
                    Text("${offer.fromName} wants to $what", style = MaterialTheme.typography.titleMedium)
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Button(onClick = { accepting = offer }) { Text("Accept") }
                        OutlinedButton(onClick = { model.decline(offer.id) }) { Text("Decline") }
                    }
                }
            }
        }
        if (status.libraries.isEmpty()) Text("No libraries yet.")
        for (lib in status.libraries) {
            RsCard(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        GlyphTile(Glyph.Folder)
                        Text(lib.name, style = MaterialTheme.typography.titleMedium)
                    }
                    Text(lib.folder, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
                    if (lib.backupOnly) Text("Encrypted backup (unreadable here)")
                    if (lib.syncingWith.isNotEmpty()) Text("Syncing with: ${lib.syncingWith.joinToString(", ")}")
                    if (lib.backedUpOn.isNotEmpty()) Text("Backed up on: ${lib.backedUpOn.joinToString(", ")}")
                    if (lib.pending.isNotEmpty()) Text("Waiting for: ${lib.pending.joinToString(", ")}")
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        if (!lib.backupOnly) {
                            OutlinedButton(onClick = { sharing = lib }, enabled = status.devices.isNotEmpty()) { Text("Share") }
                        }
                        TextButton(onClick = { removing = lib }) { Text("Remove") }
                    }
                }
            }
        }
        Button(onClick = { creating = true }) { Text("New library") }
    }

    if (creating) {
        FolderDialog(
            title = "New library",
            confirm = "Create",
            askName = true,
            onDismiss = { creating = false },
            onDone = { name, folder ->
                creating = false
                model.createLibrary(name, folder)
            },
        )
    }
    accepting?.let { offer ->
        FolderDialog(
            title = "Accept “${offer.name}”",
            confirm = "Accept",
            askName = false,
            initialFolder = if (offer.backup) status.backupDir else "",
            onDismiss = { accepting = null },
            onDone = { _, folder ->
                accepting = null
                model.accept(offer.id, folder)
            },
        )
    }
    sharing?.let { lib ->
        var backup by remember(lib.id) { mutableStateOf(false) }
        AlertDialog(
            onDismissRequest = { sharing = null },
            title = { Text("Share “${lib.name}” with…") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    for (d in status.devices) {
                        OutlinedButton(
                            modifier = Modifier.fillMaxWidth(),
                            onClick = {
                                sharing = null
                                model.share(lib.id, d.key, backup)
                            },
                        ) { Text(d.name) }
                    }
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        androidx.compose.material3.Checkbox(checked = backup, onCheckedChange = { backup = it })
                        Text("Only keep an encrypted backup there")
                    }
                }
            },
            confirmButton = {},
            dismissButton = { TextButton(onClick = { sharing = null }) { Text("Cancel") } },
        )
    }
    removing?.let { lib ->
        AlertDialog(
            onDismissRequest = { removing = null },
            title = { Text("Remove “${lib.name}”?") },
            text = { Text("It stops syncing on this phone. The files stay where they are.") },
            confirmButton = {
                TextButton(onClick = {
                    removing = null
                    model.removeLibrary(lib.id)
                }) { Text("Remove") }
            },
            dismissButton = { TextButton(onClick = { removing = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun FolderDialog(
    title: String,
    confirm: String,
    askName: Boolean,
    initialFolder: String = "",
    onDismiss: () -> Unit,
    onDone: (name: String, folder: String) -> Unit,
) {
    var name by remember { mutableStateOf("") }
    var folder by remember { mutableStateOf(initialFolder) }
    val pick = rememberFolderPicker { folder = it }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (askName) {
                    OutlinedTextField(value = name, onValueChange = { name = it }, label = { Text("Name") }, singleLine = true)
                }
                OutlinedTextField(value = folder, onValueChange = { folder = it }, label = { Text("Folder") })
                OutlinedButton(onClick = pick) { Text("Choose a folder…") }
            }
        },
        confirmButton = {
            TextButton(
                enabled = folder.isNotBlank() && (!askName || name.isNotBlank()),
                onClick = { onDone(name.trim(), folder.trim()) },
            ) { Text(confirm) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

// ---------------------------------------------------------------- Devices

@Composable
private fun Devices(model: Model, status: Status) {
    var askHost by remember { mutableStateOf(false) }
    var enter by remember { mutableStateOf(false) }
    var forgetting by remember { mutableStateOf<DeviceInfo?>(null) }
    var readdressing by remember { mutableStateOf<DeviceInfo?>(null) }
    val scan = rememberLauncherForActivityResult(ScanContract()) { result ->
        val text = result.contents
        if (text != null) model.enterCode(text)
    }

    Page("Devices", "Pair your devices once; then share libraries between them.") {
        RsCard(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Add a device", style = MaterialTheme.typography.titleMedium)
                Text("Scan the QR code the other device is showing. Or show one on this phone and scan it from there.")
                Button(modifier = Modifier.fillMaxWidth().height(52.dp), onClick = { scan.launch(scanOptions()) }) {
                    Text("Scan a code")
                }
                OutlinedButton(modifier = Modifier.fillMaxWidth(), onClick = { askHost = true }) {
                    Text("Show a code on this phone")
                }
                TextButton(modifier = Modifier.align(Alignment.CenterHorizontally), onClick = { enter = true }) {
                    Text("Enter a code by hand", style = MaterialTheme.typography.bodySmall)
                }
            }
        }
        if (status.devices.isEmpty()) Text("No devices yet.")
        for (d in status.devices) {
            RsCard(Modifier.fillMaxWidth()) {
                Row(Modifier.padding(16.dp), verticalAlignment = Alignment.CenterVertically) {
                    if (d.online) GlyphTile(Glyph.Devices, Good, GoodSoft) else GlyphTile(Glyph.Devices, Muted, MaterialTheme.colorScheme.outlineVariant)
                    Spacer(Modifier.size(12.dp))
                    Column(Modifier.weight(1f)) {
                        Text(d.name, style = MaterialTheme.typography.titleMedium)
                        Text(
                            (if (d.online) "● online" else "○ offline") + (d.addr?.let { "  $it" } ?: ""),
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                    Column(horizontalAlignment = Alignment.End) {
                        TextButton(onClick = { readdressing = d }) { Text("Change address") }
                        TextButton(onClick = { forgetting = d }) { Text("Forget") }
                    }
                }
            }
        }
        OutlinedButton(onClick = { model.resync() }) { Text("Check for new backups") }
    }

    if (askHost) {
        val options = remember { model.localAddresses().map { (a, i) -> "$a  ($i)" to a } + status.dnsNames.map { "$it  (host name)" to it } }
        var host by remember { mutableStateOf(options.firstOrNull()?.second ?: "") }
        var manual by remember { mutableStateOf(options.isEmpty()) }
        var open by remember { mutableStateOf(false) }
        AlertDialog(
            onDismissRequest = { askHost = false },
            title = { Text("Show a pairing code") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("The address the other device will use to reach this phone:")
                    Box {
                        OutlinedButton(modifier = Modifier.fillMaxWidth(), onClick = { open = true }) {
                            Text(
                                if (manual) "Set manually…"
                                else options.firstOrNull { it.second == host }?.first ?: "Choose an address",
                            )
                        }
                        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                            for ((label, value) in options) {
                                DropdownMenuItem(
                                    text = { Text(label) },
                                    onClick = {
                                        host = value
                                        manual = false
                                        open = false
                                    },
                                )
                            }
                            DropdownMenuItem(
                                text = { Text("Set manually…") },
                                onClick = {
                                    manual = true
                                    open = false
                                },
                            )
                        }
                    }
                    if (manual) {
                        OutlinedTextField(
                            value = host,
                            onValueChange = { host = it },
                            singleLine = true,
                            label = { Text("e.g. 192.168.1.20 or a host name") },
                        )
                    }
                    Text(
                        "Port ${status.port} is added for you; type :port after the address to use another.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            },
            confirmButton = {
                TextButton(
                    enabled = host.isNotBlank(),
                    onClick = {
                        askHost = false
                        model.showCode(host.trim())
                    },
                ) { Text("Create code") }
            },
            dismissButton = { TextButton(onClick = { askHost = false }) { Text("Cancel") } },
        )
    }

    if (enter) {
        var code by remember { mutableStateOf("") }
        AlertDialog(
            onDismissRequest = { enter = false },
            title = { Text("Enter a code by hand") },
            text = {
                OutlinedTextField(
                    value = code,
                    onValueChange = { code = it },
                    label = { Text("Pairing code") },
                    minLines = 3,
                )
            },
            confirmButton = {
                TextButton(
                    enabled = code.isNotBlank(),
                    onClick = {
                        enter = false
                        model.enterCode(code)
                    },
                ) { Text("Pair") }
            },
            dismissButton = { TextButton(onClick = { enter = false }) { Text("Cancel") } },
        )
    }

    readdressing?.let { d ->
        var addr by remember(d.key) { mutableStateOf(d.addr ?: "") }
        AlertDialog(
            onDismissRequest = { readdressing = null },
            title = { Text("Address of ${d.name}") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Where this phone reaches it. Change it when that device's IP address or name changed.")
                    OutlinedTextField(
                        value = addr,
                        onValueChange = { addr = it },
                        singleLine = true,
                        label = { Text("e.g. 192.168.1.20") },
                    )
                    Text(
                        "Add :port if it isn't the default (3032). Leave it empty if that device always calls this phone.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    readdressing = null
                    model.setAddress(d.key, addr.trim())
                }) { Text("Save") }
            },
            dismissButton = { TextButton(onClick = { readdressing = null }) { Text("Cancel") } },
        )
    }

    forgetting?.let { d ->
        AlertDialog(
            onDismissRequest = { forgetting = null },
            title = { Text("Forget ${d.name}?") },
            text = { Text("Libraries shared with it stay on both devices; they just stop syncing.") },
            confirmButton = {
                TextButton(onClick = {
                    forgetting = null
                    model.forget(d.key)
                }) { Text("Forget") }
            },
            dismissButton = { TextButton(onClick = { forgetting = null }) { Text("Cancel") } },
        )
    }
}

// ---------------------------------------------------------------- pairing

/** The camera screen: upright, QR codes only. */
private fun scanOptions(): ScanOptions = ScanOptions()
    .setDesiredBarcodeFormats(listOf("QR_CODE"))
    .setPrompt("Point the camera at the QR code on the other device")
    .setBeepEnabled(false)
    .setOrientationLocked(false)
    .setCaptureActivity(PortraitCaptureActivity::class.java)

@Composable
private fun QrImage(text: String, size: Dp = 240.dp) {
    val bitmap: Bitmap? = remember(text) {
        try {
            val hints = mapOf(
                EncodeHintType.MARGIN to 2,
                EncodeHintType.ERROR_CORRECTION to ErrorCorrectionLevel.L,
            )
            BarcodeEncoder().encodeBitmap(text, BarcodeFormat.QR_CODE, 800, 800, hints)
        } catch (_: Exception) {
            null
        }
    }
    if (bitmap != null) {
        Image(bitmap = bitmap.asImageBitmap(), contentDescription = "Pairing QR code", modifier = Modifier.size(size))
    } else {
        Text("This code is too long for a QR code; copy it instead.")
    }
}

@Composable
private fun PairingDialogs(model: Model) {
    val check = model.check
    val invite = model.invite
    val join = model.join
    val clipboard = LocalClipboardManager.current

    if (check != null) {
        // Comparing the code comes first: both people must agree it is the same.
        AlertDialog(
            onDismissRequest = {},
            title = { Text("Check the pairing code") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text(
                        if (check.joining) "The other device must show exactly this code:"
                        else "The device that is pairing must show exactly this code:",
                    )
                    Text(
                        check.code,
                        fontFamily = FontFamily.Monospace,
                        fontWeight = FontWeight.Bold,
                        fontSize = 34.sp,
                        color = MaterialTheme.colorScheme.primary,
                    )
                    Text("Device: ${check.name}")
                    Text("Key fingerprint", style = MaterialTheme.typography.labelMedium)
                    Text(check.fingerprint, fontFamily = FontFamily.Monospace, fontSize = 13.sp)
                    Text(
                        if (check.joining) "If the codes differ, something is wrong: choose “Not the same”."
                        else "If you did not just start pairing on a device of yours, or the codes differ, " +
                            "choose “Not the same”: someone else may have used your code.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            },
            confirmButton = { TextButton(onClick = { model.decide(true) }) { Text("The codes match: pair") } },
            dismissButton = { TextButton(onClick = { model.decide(false) }) { Text("Not the same") } },
        )
    } else if (invite != null) {
        AlertDialog(
            onDismissRequest = { model.cancelPairing() },
            title = { Text("Show a pairing code") },
            text = {
                Column(
                    Modifier.verticalScroll(rememberScrollState()),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    when {
                        invite.error != null -> Text(invite.error)
                        invite.code == null -> CircularProgressIndicator()
                        else -> {
                            Text("On the other device tap “Scan a code” and point it at this:")
                            QrImage(invite.code)
                            Text(invite.code, fontFamily = FontFamily.Monospace, fontSize = 10.sp)
                            TextButton(onClick = { clipboard.setText(AnnotatedString(invite.code)) }) { Text("Copy code") }
                            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                                Text("Waiting… (works once, for 10 minutes)", style = MaterialTheme.typography.bodySmall)
                            }
                        }
                    }
                }
            },
            confirmButton = { TextButton(onClick = { model.cancelPairing() }) { Text(if (invite.error != null) "Close" else "Cancel") } },
        )
    } else if (join != null) {
        AlertDialog(
            onDismissRequest = { model.cancelPairing() },
            title = { Text("Pairing") },
            text = {
                if (join.error != null) {
                    Text(join.error)
                } else {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
                        Text("Contacting the other device…")
                    }
                }
            },
            confirmButton = { TextButton(onClick = { model.cancelPairing() }) { Text(if (join.error != null) "Close" else "Cancel") } },
        )
    }
}
