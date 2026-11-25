package dev.waystone.app

import dev.waystone.app.input.DirectTouchEngine
import dev.waystone.app.input.InputController
import dev.waystone.app.input.PictureGeometry
import dev.waystone.app.input.Point2D
import dev.waystone.app.input.PointerOutput
import dev.waystone.app.input.Size2D
import dev.waystone.app.input.TrackpadEngine
import dev.waystone.app.input.Vector2D
import dev.waystone.app.protocol.InputEvent
import dev.waystone.app.protocol.KeyCode
import dev.waystone.app.protocol.Modifiers
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class InputEngineTest {
    @Test
    fun pictureGeometryMapsAspectFitAndZoom() {
        val geom = PictureGeometry(
            bounds = Size2D(1000f, 500f),
            videoSize = Size2D(1600f, 1000f),
        )
        // Fit height 500 -> width 800, centered at x=100..900
        assertEquals(100f, geom.contentRect.x, 0.01f)
        assertEquals(800f, geom.contentRect.width, 0.01f)

        val (nx, ny) = geom.normalized(Point2D(500f, 250f))
        assertEquals(0.5, nx, 0.001)
        assertEquals(0.5, ny, 0.001)

        val zoomed = geom.zoomed(2f, Point2D(500f, 250f), Vector2D.ZERO)
        assertTrue(zoomed.isZoomed)
        assertEquals(2f, zoomed.scale, 0.001f)
        val (zx, zy) = zoomed.normalized(Point2D(500f, 250f))
        assertEquals(0.5, zx, 0.001)
        assertEquals(0.5, zy, 0.001)
    }

    @Test
    fun directTouchDistinguishesClickHoverAndGrab() {
        val engine = DirectTouchEngine()
        // Quick tap -> click
        assertTrue(engine.began(1, Point2D(100f, 100f), 1.0).isEmpty())
        val tapOut = engine.ended(1, Point2D(101f, 100f), 1.05)
        assertTrue(tapOut.any { it is PointerOutput.Click && it.button == InputEvent.MouseButton.LEFT })

        // Move past slop -> hover only, lift does not click
        engine.began(2, Point2D(100f, 100f), 2.0)
        val hoverOut = engine.moved(mapOf(2 to Point2D(120f, 100f)), 2.05)
        assertEquals(listOf(PointerOutput.Move(Point2D(120f, 100f))), hoverOut)
        assertTrue(engine.ended(2, Point2D(120f, 100f), 2.1).isEmpty())

        // Hold past holdDuration -> grab + drag + release
        engine.began(3, Point2D(50f, 50f), 3.0)
        val grabOut = engine.tick(3.2)
        assertTrue(grabOut.any { it is PointerOutput.Press && it.button == InputEvent.MouseButton.LEFT })
        val releaseOut = engine.ended(3, Point2D(80f, 50f), 3.4)
        assertTrue(releaseOut.any { it is PointerOutput.Release && it.button == InputEvent.MouseButton.LEFT })
    }

    @Test
    fun trackpadMovesRelativelyAndRightClicksWithTwoFingers() {
        val engine = TrackpadEngine()
        engine.began(1, Point2D(100f, 100f), 1.0)
        val moveOut = engine.moved(mapOf(1 to Point2D(120f, 100f)), 1.05)
        assertTrue(moveOut.single() is PointerOutput.MoveBy)
        assertTrue(engine.ended(1, Point2D(120f, 100f), 1.08).isEmpty())

        // Two-finger quick tap -> right click
        engine.began(1, Point2D(100f, 100f), 2.0)
        engine.began(2, Point2D(140f, 100f), 2.01)
        val rightClick = engine.ended(2, Point2D(140f, 100f), 2.08)
        assertTrue(rightClick.any { it is PointerOutput.Click && it.button == InputEvent.MouseButton.RIGHT })
    }

    @Test
    fun inputControllerArmsModifiersAndReleasesAll() {
        val events = mutableListOf<InputEvent>()
        val controller = InputController(InputController.Mode.DIRECT)
        controller.sink = { events.add(it) }

        controller.toggleArmed(Modifiers.CONTROL)
        controller.type("c")
        assertTrue(events.any { it == InputEvent.Key(KeyCode.letter('c')!!, down = true, modifiers = Modifiers.CONTROL) })
        assertEquals(Modifiers.NONE, controller.armedModifiers.value)

        events.clear()
        controller.releaseAll()
        assertEquals(listOf(InputEvent.ReleaseAll), events)
        assertFalse(controller.directPointerActive.value)
    }
}
