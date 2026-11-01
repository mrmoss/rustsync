package app.rustsync

import android.net.LocalSocket
import android.net.LocalSocketAddress
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStream

// What the daemon says about itself. These mirror src/proto.rs in the Rust core: the control
// socket speaks one JSON value per line.

data class DeviceInfo(
    val key: String,
    val name: String,
    val online: Boolean,
    val addr: String?,
    val lastSeen: Long,
    val upRate: Long = 0L,
    val downRate: Long = 0L,
)

/** How a library is syncing with one device: what is still to be fetched, each way. */
data class SyncInfo(
    val key: String,
    val device: String,
    val online: Boolean,
    val downFiles: Long,
    val downBytes: Long,
    val upFiles: Long,
    val upBytes: Long,
) {
    fun busy() = downFiles > 0 || downBytes > 0 || upFiles > 0 || upBytes > 0
}

data class LibraryInfo(
    val id: String,
    val name: String,
    val folder: String,
    val backupOnly: Boolean,
    val syncingWith: List<String>,
    val backedUpOn: List<String>,
    val pending: List<String>,
    val used: Long,
    val quota: Long,
    val sync: List<SyncInfo> = emptyList(),
)

data class OfferInfo(val id: String, val name: String, val fromName: String, val backup: Boolean, val again: Boolean)

data class Status(
    val name: String,
    val key: String,
    val listen: String,
    val backupDir: String,
    val port: Int,
    val dnsNames: List<String>,
    val logMax: Long,
    val devices: List<DeviceInfo>,
    val libraries: List<LibraryInfo>,
    val offers: List<OfferInfo>,
)

/** What a person compares while pairing. */
data class PairCheck(val name: String, val fingerprint: String, val code: String, val joining: Boolean)

sealed class Reply {
    data class Info(val status: Status) : Reply()
    data class Out(val text: String) : Reply()
    data class Note(val text: String) : Reply()
    data class Verify(val check: PairCheck) : Reply()
    data object Done : Reply()
    data class Error(val message: String) : Reply()
}

private fun JSONArray?.strings(): List<String> {
    if (this == null) return emptyList()
    return (0 until length()).map { getString(it) }
}

private fun parseStatus(o: JSONObject): Status {
    val devices = o.optJSONArray("devices")
    val libraries = o.optJSONArray("libraries")
    val offers = o.optJSONArray("offers")
    return Status(
        name = o.optString("name"),
        key = o.optString("key"),
        listen = o.optString("listen"),
        backupDir = o.optString("backup_dir"),
        port = o.optInt("port", 3032),
        dnsNames = o.optJSONArray("dns_names").strings(),
        logMax = o.optLong("log_max", 2_000_000L),
        devices = (0 until (devices?.length() ?: 0)).map {
            val d = devices!!.getJSONObject(it)
            DeviceInfo(
                key = d.optString("key"),
                name = d.optString("name"),
                online = d.optBoolean("online"),
                addr = if (d.isNull("addr")) null else d.optString("addr"),
                lastSeen = d.optLong("last_seen", 0L),
                upRate = d.optLong("up_rate", 0L),
                downRate = d.optLong("down_rate", 0L),
            )
        },
        libraries = (0 until (libraries?.length() ?: 0)).map {
            val l = libraries!!.getJSONObject(it)
            LibraryInfo(
                id = l.optString("id"),
                name = l.optString("name"),
                folder = l.optString("folder"),
                backupOnly = l.optBoolean("backup_only"),
                syncingWith = l.optJSONArray("syncing_with").strings(),
                backedUpOn = l.optJSONArray("backed_up_on").strings(),
                pending = l.optJSONArray("pending").strings(),
                used = l.optLong("used"),
                quota = l.optLong("quota"),
                sync = (0 until (l.optJSONArray("sync")?.length() ?: 0)).map { n ->
                    val s = l.getJSONArray("sync").getJSONObject(n)
                    SyncInfo(
                        key = s.optString("key"),
                        device = s.optString("device"),
                        online = s.optBoolean("online"),
                        downFiles = s.optLong("down_files"),
                        downBytes = s.optLong("down_bytes"),
                        upFiles = s.optLong("up_files"),
                        upBytes = s.optLong("up_bytes"),
                    )
                },
            )
        },
        offers = (0 until (offers?.length() ?: 0)).map {
            val f = offers!!.getJSONObject(it)
            OfferInfo(
                id = f.optString("id"),
                name = f.optString("name"),
                fromName = f.optString("from_name"),
                backup = f.optBoolean("backup"),
                again = f.optBoolean("again"),
            )
        },
    )
}

fun parseReply(line: String): Reply {
    val v = JSONTokener(line).nextValue()
    if (v is String) return if (v == "Done") Reply.Done else Reply.Note(v)
    val o = v as JSONObject
    return when {
        o.has("Info") -> Reply.Info(parseStatus(o.getJSONObject("Info")))
        o.has("Out") -> Reply.Out(o.getString("Out"))
        o.has("Note") -> Reply.Note(o.getString("Note"))
        o.has("Error") -> Reply.Error(o.getString("Error"))
        o.has("Verify") -> {
            val x = o.getJSONObject("Verify")
            Reply.Verify(
                PairCheck(
                    name = x.optString("name"),
                    fingerprint = x.optString("fingerprint"),
                    code = x.optString("code"),
                    joining = x.optBoolean("joining"),
                ),
            )
        }
        else -> Reply.Note("")
    }
}

/** The requests, as the daemon reads them (serde's default enum layout). */
object Req {
    private fun obj(name: String, body: JSONObject) = JSONObject().put(name, body).toString()

    fun status() = "\"Status\""
    fun refresh() = "\"Refresh\""
    fun invite(host: String) = obj("Invite", JSONObject().put("host", host))
    fun join(token: String) = obj("Join", JSONObject().put("token", token))
    fun decision(accept: Boolean) = obj("PairDecision", JSONObject().put("accept", accept))
    fun setPort(port: Int) = obj("SetPort", JSONObject().put("port", port))
    fun log(device: String?, library: String?, lines: Int) =
        obj(
            "Log",
            JSONObject()
                .put("device", device ?: JSONObject.NULL)
                .put("library", library ?: JSONObject.NULL)
                .put("lines", lines),
        )
    fun setLogSize(bytes: Long) = obj("SetLogSize", JSONObject().put("bytes", bytes))
    fun forget(device: String) = obj("Forget", JSONObject().put("device", device))
    fun setAddress(device: String, addr: String) =
        obj("SetAddress", JSONObject().put("device", device).put("addr", addr))
    fun create(name: String, folder: String) = obj("Create", JSONObject().put("name", name).put("folder", folder))
    fun removeLibrary(library: String) = obj("RemoveLibrary", JSONObject().put("library", library))
    fun share(library: String, device: String, backup: Boolean) =
        obj("Share", JSONObject().put("library", library).put("device", device).put("backup", backup))
    fun accept(offer: String, folder: String, quota: Long) =
        obj("Accept", JSONObject().put("offer", offer).put("folder", folder).put("quota", quota))
    fun decline(offer: String) = obj("Decline", JSONObject().put("offer", offer))
}

/** One conversation with the daemon: a request, then answers until Done or Error. */
class Session(socketPath: String, request: String) : AutoCloseable {
    private val socket = LocalSocket()
    private val out: OutputStream
    private val input: BufferedReader

    init {
        socket.connect(LocalSocketAddress(socketPath, LocalSocketAddress.Namespace.FILESYSTEM))
        out = socket.outputStream
        input = BufferedReader(InputStreamReader(socket.inputStream, Charsets.UTF_8))
        send(request)
    }

    @Synchronized
    fun send(json: String) {
        out.write((json + "\n").toByteArray(Charsets.UTF_8))
        out.flush()
    }

    /** The next answer, or null when the daemon closed the connection. */
    fun next(): Reply? {
        val line = input.readLine() ?: return null
        return parseReply(line)
    }

    /** Closing the connection while an invite waits cancels it. */
    override fun close() {
        try {
            socket.close()
        } catch (_: Exception) {
        }
    }
}

class Control(private val dataDir: String) {
    fun open(request: String): Session = Session(Core.socketPath(dataDir), request)

    fun status(): Status {
        open(Req.status()).use { s ->
            while (true) {
                val r = s.next() ?: break
                when (r) {
                    is Reply.Info -> return r.status
                    is Reply.Error -> error(r.message)
                    else -> {}
                }
            }
        }
        error("RustSync did not answer")
    }

    /** Run a one-shot request; returns what it printed. Throws with the daemon's message. */
    fun run(request: String): String {
        val text = StringBuilder()
        open(request).use { s ->
            while (true) {
                when (val r = s.next() ?: break) {
                    is Reply.Out -> text.append(r.text)
                    is Reply.Error -> error(r.message)
                    Reply.Done -> break
                    else -> {}
                }
            }
        }
        return text.toString()
    }
}
