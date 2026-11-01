package app.rustsync

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Fill
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import kotlin.math.cos
import kotlin.math.sin

// One palette for every RustSync app (the desktop window uses the same values): a neutral,
// slightly warm graphite with the logo's orange as the only accent.
val Accent = Color(0xFFFF8A4C)
val AccentSoft = Color(0xFF34221A)
val Good = Color(0xFF52C98C)
val GoodSoft = Color(0xFF193228)
val Muted = Color(0xFF9696A2)

private val Dark = darkColorScheme(
    primary = Accent,
    onPrimary = Color(0xFF1C1008),
    primaryContainer = AccentSoft,
    onPrimaryContainer = Color(0xFFFFB48A),
    secondary = Muted,
    onSecondary = Color(0xFF121214),
    secondaryContainer = Color(0xFF2A2A30),
    onSecondaryContainer = Color(0xFFE2E2E8),
    tertiary = Good,
    background = Color(0xFF121214),
    onBackground = Color(0xFFE2E2E8),
    surface = Color(0xFF121214),
    onSurface = Color(0xFFE2E2E8),
    surfaceVariant = Color(0xFF1B1B1F),
    onSurfaceVariant = Muted,
    surfaceContainer = Color(0xFF0E0E10),
    surfaceContainerHigh = Color(0xFF1B1B1F),
    surfaceContainerHighest = Color(0xFF1B1B1F),
    outline = Color(0xFF3A3A42),
    outlineVariant = Color(0xFF2C2C32),
    error = Color(0xFFF06262),
)

private val Light = lightColorScheme(
    primary = Color(0xFFD9541E),
    onPrimary = Color.White,
    primaryContainer = Color(0xFFFFE6DA),
    onPrimaryContainer = Color(0xFF5A1D08),
    secondary = Color(0xFF6B6B76),
    secondaryContainer = Color(0xFFEDEDF0),
    onSecondaryContainer = Color(0xFF1B1B1F),
    tertiary = Color(0xFF1E8F5E),
    background = Color(0xFFFAFAFB),
    onBackground = Color(0xFF1B1B1F),
    surface = Color(0xFFFAFAFB),
    onSurface = Color(0xFF1B1B1F),
    surfaceVariant = Color(0xFFF1F1F4),
    onSurfaceVariant = Color(0xFF6B6B76),
    surfaceContainer = Color(0xFFFFFFFF),
    surfaceContainerHigh = Color(0xFFF1F1F4),
    surfaceContainerHighest = Color(0xFFF1F1F4),
    outline = Color(0xFFB9B9C2),
    outlineVariant = Color(0xFFE2E2E8),
    error = Color(0xFFD14343),
)

@Composable
fun RustSyncTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = if (isSystemInDarkTheme()) Dark else Light, content = content)
}

/** A card with a thin outline, like the desktop window's cards. */
@Composable
fun RsCard(modifier: Modifier = Modifier, content: @Composable ColumnScope.() -> Unit) {
    Card(
        modifier = modifier,
        shape = RoundedCornerShape(14.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
        content = content,
    )
}

// ---- icons, drawn with lines so they look the same everywhere (and the same as on the desktop)

enum class Glyph { Home, Folder, Devices, Plus, Refresh }

@Composable
fun GlyphIcon(glyph: Glyph, modifier: Modifier = Modifier.size(24.dp), color: Color = LocalContentColor.current) {
    Canvas(modifier) {
        val s = size.minDimension
        fun at(x: Float, y: Float) = Offset(x * s, y * s)
        val stroke = Stroke(width = s / 11f, cap = StrokeCap.Round, join = StrokeJoin.Round)
        fun poly(p: List<Offset>, closed: Boolean = false) {
            val path = Path()
            path.moveTo(p[0].x, p[0].y)
            for (i in 1 until p.size) path.lineTo(p[i].x, p[i].y)
            if (closed) path.close()
            drawPath(path, color, style = stroke)
        }
        when (glyph) {
            Glyph.Home -> {
                poly(listOf(at(0.10f, 0.50f), at(0.50f, 0.14f), at(0.90f, 0.50f)))
                poly(listOf(at(0.22f, 0.42f), at(0.22f, 0.86f), at(0.78f, 0.86f), at(0.78f, 0.42f)))
                poly(listOf(at(0.42f, 0.86f), at(0.42f, 0.60f), at(0.58f, 0.60f), at(0.58f, 0.86f)))
            }
            Glyph.Folder -> poly(
                listOf(at(0.10f, 0.22f), at(0.40f, 0.22f), at(0.50f, 0.34f), at(0.90f, 0.34f), at(0.90f, 0.80f), at(0.10f, 0.80f)),
                closed = true,
            )
            Glyph.Devices -> {
                poly(listOf(at(0.10f, 0.20f), at(0.90f, 0.20f), at(0.90f, 0.64f), at(0.10f, 0.64f)), closed = true)
                poly(listOf(at(0.50f, 0.64f), at(0.50f, 0.82f)))
                poly(listOf(at(0.30f, 0.84f), at(0.70f, 0.84f)))
            }
            Glyph.Plus -> {
                poly(listOf(at(0.50f, 0.18f), at(0.50f, 0.82f)))
                poly(listOf(at(0.18f, 0.50f), at(0.82f, 0.50f)))
            }
            Glyph.Refresh -> {
                drawArc(
                    color = color,
                    startAngle = 50f,
                    sweepAngle = 280f,
                    useCenter = false,
                    topLeft = at(0.18f, 0.18f),
                    size = Size(0.64f * s, 0.64f * s),
                    style = stroke,
                )
                val a = Math.toRadians(330.0)
                val c = cos(a).toFloat()
                val n = sin(a).toFloat()
                val ex = 0.5f + 0.32f * c
                val ey = 0.5f + 0.32f * n
                val head = Path()
                val tip = at(ex - n * 0.17f, ey + c * 0.17f)
                val l = at(ex + c * 0.13f, ey + n * 0.13f)
                val r = at(ex - c * 0.13f, ey - n * 0.13f)
                head.moveTo(tip.x, tip.y)
                head.lineTo(l.x, l.y)
                head.lineTo(r.x, r.y)
                head.close()
                drawPath(head, color, style = Fill)
            }
        }
    }
}

/** A rounded square with an icon, for the head of a card. */
@Composable
fun GlyphTile(glyph: Glyph, color: Color = Accent, fill: Color = AccentSoft, size: Dp = 42.dp) {
    Box(Modifier.size(size).background(fill, RoundedCornerShape(11.dp)), contentAlignment = Alignment.Center) {
        GlyphIcon(glyph, Modifier.size(size - 20.dp), color)
    }
}