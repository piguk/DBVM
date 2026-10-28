// 16-input priority encoder: y is the index of the highest set bit of x; valid is 0 (and y 0) when x is 0.
module prio_enc(input [15:0] x, output reg [3:0] y, output valid);
  integer i;
  always @(*) begin
    y = 4'd0;
    for (i = 0; i < 16; i = i + 1)
      if (x[i]) y = i[3:0];
  end
  assign valid = (x != 16'd0);
endmodule
