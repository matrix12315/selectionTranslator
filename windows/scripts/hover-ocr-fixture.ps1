<# Forced-OCR fixture: owner-drawn control has no UIA TextPattern. #>
[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$RunRoot)
$ErrorActionPreference='Stop'
if([Threading.Thread]::CurrentThread.GetApartmentState() -ne [Threading.ApartmentState]::STA){throw 'fixture requires STA'}
Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing
New-Item -ItemType Directory -Force -Path $RunRoot | Out-Null
$ready=Join-Path $RunRoot 'hover-ocr-fixture-ready.json'; $stop=Join-Path $RunRoot 'hover-ocr-fixture-stop'
$sentence='The hovered word needs context for a precise explanation.'
$form=New-Object Windows.Forms.Form; $form.Text='Selection Translate - OCR fixture'; $form.StartPosition='Manual'; $form.Location=[Drawing.Point]::new(40,180); $form.ClientSize=[Drawing.Size]::new(1120,260); $form.FormBorderStyle='FixedSingle'; $form.TopMost=$true; $form.AutoScaleMode=[Windows.Forms.AutoScaleMode]::None
$panel=New-Object Windows.Forms.Panel; $panel.Location=[Drawing.Point]::new(20,20); $panel.Size=[Drawing.Size]::new(1080,210); $panel.BackColor=[Drawing.Color]::White
$panel.Add_Paint({ param($s,$e); $e.Graphics.TextRenderingHint=[Drawing.Text.TextRenderingHint]::SingleBitPerPixelGridFit; $f=[Drawing.Font]::new('Segoe UI',28,[Drawing.FontStyle]::Regular); $b=[Drawing.Brushes]::Black; $e.Graphics.DrawString('The hovered word needs context',$f,$b,160,25); $e.Graphics.DrawString('for a precise explanation.',$f,$b,160,82); $e.Graphics.DrawString('Unrelated far-column sentence.',$f,$b,790,25); $f.Dispose() })
[void]$form.Controls.Add($panel); $form.Show(); $form.Activate(); $form.BringToFront(); [Windows.Forms.Application]::DoEvents()
$g=$panel.CreateGraphics(); $font=[Drawing.Font]::new('Segoe UI',28,[Drawing.FontStyle]::Regular)
$targetSize=$g.MeasureString('hovered',$font); $lineOrigin=[Drawing.Point]::new(160,25); $prefix=$g.MeasureString('The ',$font)
$targetLocal=[Drawing.Point]::new([int]($lineOrigin.X+$prefix.Width+$targetSize.Width/2),[int]($lineOrigin.Y+$targetSize.Height/2)); $contextLocal=[Drawing.Point]::new(400,105); $blankLocal=[Drawing.Point]::new(680,180); $farLocal=[Drawing.Point]::new(900,48)
$target=$panel.PointToScreen($targetLocal); $context=$panel.PointToScreen($contextLocal); $blank=$panel.PointToScreen($blankLocal); $far=$panel.PointToScreen($farLocal); $g.Dispose(); $font.Dispose()
[pscustomobject]@{pid=$PID;hwnd=$form.Handle.ToInt64();control_hwnd=$panel.Handle.ToInt64();uia_text_pattern=$false;expected=$sentence;target='hovered';context='context';target_x=$target.X;target_y=$target.Y;context_x=$context.X;context_y=$context.Y;blank_x=$blank.X;blank_y=$blank.Y;far_x=$far.X;far_y=$far.Y;line1='The hovered word needs context';line2='for a precise explanation.';far_sentence='Unrelated far-column sentence.'}|ConvertTo-Json -Compress | Set-Content -Encoding UTF8 $ready
try { while(-not (Test-Path $stop)){[Windows.Forms.Application]::DoEvents();Start-Sleep -Milliseconds 20} } finally { $form.Close();$form.Dispose() }
