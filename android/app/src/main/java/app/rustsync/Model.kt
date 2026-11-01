package app.rustsync

import android.app.Application
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.net.Inet4Address
import java.net.NetworkInterface

/** Showing a pairing code for another device to use. */
data class InviteUi(val code: String?, val error: String?)

/** Entering (or scanning) a pairing code from another device. */
data class JoinUi(val busy: Boolean, val error: String?)

class Model(app: Application) : AndroidViewModel(app) {
    private val control = Control(Paths.dataDir(app))

    var status by mutableStateOf<Status?>(null)
        private set
    var offline by mutableStateOf<String?>("Starting…")
        private set
    var message by mutableStateOf<String?>(null)
    var invite by mutableStateOf<InviteUi?>(null)
        private set
    var join by mutableStateOf<JoinUi?>(null)
        private set

    /** Set while the person has to compare the pairing code with the other device. */
    var check by mutableStateOf<PairCheck?>(null)
        private set

    private var session: Session? = null

    init {
        viewModelScope.launch(Dispatchers.IO) {
            while (isActive) {
                refresh()
                delay(2000)
            }
        }
    }

    fun refresh() {
        try {
            status = control.status()
            offline = null
        } catch (e: Exception) {
            status = null
            offline = e.message ?: "RustSync is not running"
        }
    }

    /** Run a request in the background and show the result. */
    fun run(request: String, then: () -> Unit = {}) {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val out = control.run(request)
                if (out.isNotBlank()) message = out
                then()
            } catch (e: Exception) {
                message = e.message ?: "something went wrong"
            }
            refresh()
        }
    }

    // ---------------------------------------------------------------- pairing

    fun showCode(host: String) {
        closeSession()
        invite = InviteUi(null, null)
        talk(Req.invite(host)) { reply ->
            when (reply) {
                is Reply.Out -> invite = invite?.copy(code = reply.text)
                is Reply.Note -> if (reply.text.startsWith("Paired with")) message = reply.text
                is Reply.Error -> {
                    invite = InviteUi(null, reply.message)
                    check = null
                }
                Reply.Done -> {
                    invite = null
                    check = null
                }
                else -> {}
            }
        }
    }

    fun enterCode(token: String) {
        closeSession()
        join = JoinUi(true, null)
        talk(Req.join(token.trim())) { reply ->
            when (reply) {
                is Reply.Out -> message = reply.text
                is Reply.Error -> {
                    join = JoinUi(false, reply.message)
                    check = null
                }
                Reply.Done -> {
                    join = null
                    check = null
                }
                else -> {}
            }
        }
    }

    private fun talk(request: String, onReply: (Reply) -> Unit) {
        viewModelScope.launch(Dispatchers.IO) {
            var s: Session? = null
            try {
                s = control.open(request)
                session = s
                while (true) {
                    val reply = s.next() ?: break
                    if (session !== s) break // cancelled or replaced meanwhile
                    if (reply is Reply.Verify) check = reply.check
                    onReply(reply)
                    if (reply is Reply.Done || reply is Reply.Error) break
                }
            } catch (e: Exception) {
                if (session === s) {
                    val problem = e.message ?: "something went wrong"
                    if (invite != null) invite = InviteUi(null, problem)
                    if (join != null) join = JoinUi(false, problem)
                }
            } finally {
                s?.close()
            }
            refresh()
        }
    }

    /** The person compared the codes: yes or no. */
    fun decide(accept: Boolean) {
        check = null
        val s = session ?: return
        viewModelScope.launch(Dispatchers.IO) {
            try {
                s.send(Req.decision(accept))
            } catch (_: Exception) {
            }
        }
    }

    fun cancelPairing() {
        closeSession()
        invite = null
        join = null
        check = null
    }

    fun clearJoinError() {
        join = null
    }

    private fun closeSession() {
        session?.close()
        session = null
    }

    override fun onCleared() {
        closeSession()
    }

    // ---------------------------------------------------------------- everything else

    fun forget(device: String) = run(Req.forget(device))
    fun createLibrary(name: String, folder: String) = run(Req.create(name, folder))
    fun removeLibrary(id: String) = run(Req.removeLibrary(id))
    fun share(library: String, device: String, backup: Boolean) = run(Req.share(library, device, backup))
    fun accept(offer: String, folder: String) = run(Req.accept(offer, folder, 0))
    fun decline(offer: String) = run(Req.decline(offer))
    fun resync() = run(Req.refresh())

    /** A guess at this phone's address on the local network, to prefill the pairing dialog. */
    fun localAddress(): String {
        try {
            for (nif in NetworkInterface.getNetworkInterfaces()) {
                if (nif.isLoopback || !nif.isUp) continue
                for (addr in nif.inetAddresses) {
                    if (addr is Inet4Address && addr.isSiteLocalAddress) return addr.hostAddress ?: ""
                }
            }
        } catch (_: Exception) {
        }
        return ""
    }
}
