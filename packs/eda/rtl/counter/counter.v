// 8-bit up/down counter: synchronous reset to 0; load sets q to d; otherwise, when en, counts up (up = 1)
// or down, wrapping around.
module counter(input clk, input rst, input en, input up, input load, input [7:0] d, output reg [7:0] q);
  always @(posedge clk) begin
    if (rst) q <= 8'd0;
    else if (load) q <= d;
    else if (en) q <= up ? q + 8'd1 : q - 8'd1;
  end
endmodule
