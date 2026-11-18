package app.localtugboat

import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.width
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.Dp

/** The logo's ink (assets/brand/README.md): navy on a light surface, white on a dark one. */
val LogoInkLight = Color(0xFF1A2733)
val LogoInkDark = Color(0xFFFFFFFF)

/** Logo shape: 904 x 362 source px (drawable ic_tugboat_logo, the owner's artwork traced to vectors). */
const val LOGO_RATIO = 904f / 362f

/** The logo ink for a surface colour: dark surface -> white, light surface -> navy. Pure, so the test can pin it. */
fun logoInkFor(surface: Color): Color = if (surface.luminance() < 0.5f) LogoInkDark else LogoInkLight

/**
 * The Tugboat logo for empty states and the first-run screen, [width] wide (height follows the artwork's
 * 904:362 shape). Decorative: the words beside it carry the meaning, so it has no content description.
 * The leftmost porthole is cut open by the hull's bottom edge ON PURPOSE (the owner's signature detail):
 * the vector is the owner's artwork unchanged.
 */
@Composable
fun TugboatArt(width: Dp, modifier: Modifier = Modifier) {
    Image(
        painter = painterResource(R.drawable.ic_tugboat_logo),
        contentDescription = null,
        modifier = modifier.width(width).aspectRatio(LOGO_RATIO),
        contentScale = ContentScale.Fit,
        colorFilter = ColorFilter.tint(logoInkFor(MaterialTheme.colorScheme.surface)),
    )
}
